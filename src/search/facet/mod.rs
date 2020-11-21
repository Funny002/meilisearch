use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::Bound::{self, Unbounded, Included, Excluded};

use heed::types::{ByteSlice, DecodeIgnore};
use log::debug;
use num_traits::Bounded;
use parser::{PREC_CLIMBER, FilterParser};
use pest::error::{Error as PestError, ErrorVariant};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use roaring::RoaringBitmap;

use crate::facet::FacetType;
use crate::heed_codec::facet::FacetValueStringCodec;
use crate::heed_codec::facet::{FacetLevelValueI64Codec, FacetLevelValueF64Codec};
use crate::{Index, FieldsIdsMap, CboRoaringBitmapCodec};

use self::FacetCondition::*;
use self::FacetNumberOperator::*;
use self::parser::Rule;

mod parser;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum FacetNumberOperator<T> {
    GreaterThan(T),
    GreaterThanOrEqual(T),
    LowerThan(T),
    LowerThanOrEqual(T),
    Equal(T),
    Between(T, T),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FacetStringOperator {
    Equal(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FacetCondition {
    OperatorI64(u8, FacetNumberOperator<i64>),
    OperatorF64(u8, FacetNumberOperator<f64>),
    OperatorString(u8, FacetStringOperator),
    Or(Box<Self>, Box<Self>),
    And(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

fn get_field_id_facet_type<'a>(
    fields_ids_map: &FieldsIdsMap,
    faceted_fields: &HashMap<u8, FacetType>,
    items: &mut Pairs<'a, Rule>,
) -> Result<(u8, FacetType), PestError<Rule>>
{
    // lexing ensures that we at least have a key
    let key = items.next().unwrap();
    let field_id = fields_ids_map
        .id(key.as_str())
        .ok_or_else(|| {
            PestError::new_from_span(
                ErrorVariant::CustomError {
                    message: format!(
                        "attribute `{}` not found, available attributes are: {}",
                        key.as_str(),
                        fields_ids_map.iter().map(|(_, n)| n).collect::<Vec<_>>().join(", ")
                    ),
                },
                key.as_span(),
            )
        })?;

    let facet_type = faceted_fields
        .get(&field_id)
        .copied()
        .ok_or_else(|| {
            PestError::new_from_span(
                ErrorVariant::CustomError {
                    message: format!(
                        "attribute `{}` is not faceted, available faceted attributes are: {}",
                        key.as_str(),
                        faceted_fields.keys().flat_map(|id| fields_ids_map.name(*id)).collect::<Vec<_>>().join(", ")
                    ),
                },
                key.as_span(),
            )
        })?;

    Ok((field_id, facet_type))
}

impl FacetCondition {
    pub fn from_str(
        rtxn: &heed::RoTxn,
        index: &Index,
        expression: &str,
    ) -> anyhow::Result<FacetCondition>
    {
        let fields_ids_map = index.fields_ids_map(rtxn)?;
        let faceted_fields = index.faceted_fields(rtxn)?;
        let lexed = FilterParser::parse(Rule::prgm, expression)?;
        FacetCondition::from_pairs(&fields_ids_map, &faceted_fields, lexed)
    }

    fn from_pairs(
        fim: &FieldsIdsMap,
        ff: &HashMap<u8, FacetType>,
        expression: Pairs<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        PREC_CLIMBER.climb(
            expression,
            |pair: Pair<Rule>| match pair.as_rule() {
                Rule::between => Ok(FacetCondition::between(fim, ff, pair)?),
                Rule::eq => Ok(FacetCondition::equal(fim, ff, pair)?),
                Rule::neq => Ok(Not(Box::new(FacetCondition::equal(fim, ff, pair)?))),
                Rule::greater => Ok(FacetCondition::greater_than(fim, ff, pair)?),
                Rule::geq => Ok(FacetCondition::greater_than_or_equal(fim, ff, pair)?),
                Rule::less => Ok(FacetCondition::lower_than(fim, ff, pair)?),
                Rule::leq => Ok(FacetCondition::lower_than_or_equal(fim, ff, pair)?),
                Rule::prgm => Self::from_pairs(fim, ff, pair.into_inner()),
                Rule::term => Self::from_pairs(fim, ff, pair.into_inner()),
                Rule::not => Ok(Not(Box::new(Self::from_pairs(fim, ff, pair.into_inner())?))),
                _ => unreachable!(),
            },
            |lhs: anyhow::Result<FacetCondition>, op: Pair<Rule>, rhs: anyhow::Result<FacetCondition>| {
                match op.as_rule() {
                    Rule::or => Ok(Or(Box::new(lhs?), Box::new(rhs?))),
                    Rule::and => Ok(And(Box::new(lhs?), Box::new(rhs?))),
                    _ => unreachable!(),
                }
            },
        )
    }

    fn between(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let item_span = item.as_span();
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let lvalue = items.next().unwrap();
        let rvalue = items.next().unwrap();
        match ftype {
            FacetType::Integer => {
                let lvalue = lvalue.as_str().parse()?;
                let rvalue = rvalue.as_str().parse()?;
                Ok(OperatorI64(fid, Between(lvalue, rvalue)))
            },
            FacetType::Float => {
                let lvalue = lvalue.as_str().parse()?;
                let rvalue = rvalue.as_str().parse()?;
                Ok(OperatorF64(fid, Between(lvalue, rvalue)))
            },
            FacetType::String => {
                Err(PestError::<Rule>::new_from_span(
                    ErrorVariant::CustomError {
                        message: format!("invalid operator on a faceted string"),
                    },
                    item_span,
                ).into())
            },
        }
    }

    fn equal(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let value = items.next().unwrap();
        match ftype {
            FacetType::Integer => Ok(OperatorI64(fid, Equal(value.as_str().parse()?))),
            FacetType::Float => Ok(OperatorF64(fid, Equal(value.as_str().parse()?))),
            FacetType::String => {
                Ok(OperatorString(fid, FacetStringOperator::Equal(value.as_str().to_string())))
            },
        }
    }

    fn greater_than(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let item_span = item.as_span();
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let value = items.next().unwrap();
        match ftype {
            FacetType::Integer => Ok(OperatorI64(fid, GreaterThan(value.as_str().parse()?))),
            FacetType::Float => Ok(OperatorF64(fid, GreaterThan(value.as_str().parse()?))),
            FacetType::String => {
                Err(PestError::<Rule>::new_from_span(
                    ErrorVariant::CustomError {
                        message: format!("invalid operator on a faceted string"),
                    },
                    item_span,
                ).into())
            },
        }
    }

    fn greater_than_or_equal(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let item_span = item.as_span();
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let value = items.next().unwrap();
        match ftype {
            FacetType::Integer => Ok(OperatorI64(fid, GreaterThanOrEqual(value.as_str().parse()?))),
            FacetType::Float => Ok(OperatorF64(fid, GreaterThanOrEqual(value.as_str().parse()?))),
            FacetType::String => {
                Err(PestError::<Rule>::new_from_span(
                    ErrorVariant::CustomError {
                        message: format!("invalid operator on a faceted string"),
                    },
                    item_span,
                ).into())
            },
        }
    }

    fn lower_than(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let item_span = item.as_span();
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let value = items.next().unwrap();
        match ftype {
            FacetType::Integer => Ok(OperatorI64(fid, LowerThan(value.as_str().parse()?))),
            FacetType::Float => Ok(OperatorF64(fid, LowerThan(value.as_str().parse()?))),
            FacetType::String => {
                Err(PestError::<Rule>::new_from_span(
                    ErrorVariant::CustomError {
                        message: format!("invalid operator on a faceted string"),
                    },
                    item_span,
                ).into())
            },
        }
    }

    fn lower_than_or_equal(
        fields_ids_map: &FieldsIdsMap,
        faceted_fields: &HashMap<u8, FacetType>,
        item: Pair<Rule>,
    ) -> anyhow::Result<FacetCondition>
    {
        let item_span = item.as_span();
        let mut items = item.into_inner();
        let (fid, ftype) = get_field_id_facet_type(fields_ids_map, faceted_fields, &mut items)?;
        let value = items.next().unwrap();
        match ftype {
            FacetType::Integer => Ok(OperatorI64(fid, LowerThanOrEqual(value.as_str().parse()?))),
            FacetType::Float => Ok(OperatorF64(fid, LowerThanOrEqual(value.as_str().parse()?))),
            FacetType::String => {
                Err(PestError::<Rule>::new_from_span(
                    ErrorVariant::CustomError {
                        message: format!("invalid operator on a faceted string"),
                    },
                    item_span,
                ).into())
            },
        }
    }
}

impl FacetCondition {
    /// Aggregates the documents ids that are part of the specified range automatically
    /// going deeper through the levels.
    fn explore_facet_levels<'t, T: 't, KC>(
        rtxn: &'t heed::RoTxn,
        db: heed::Database<ByteSlice, CboRoaringBitmapCodec>,
        field_id: u8,
        level: u8,
        left: Bound<T>,
        right: Bound<T>,
        output: &mut RoaringBitmap,
    ) -> anyhow::Result<()>
    where
        T: Copy + PartialEq + PartialOrd + Bounded + Debug,
        KC: heed::BytesDecode<'t, DItem = (u8, u8, T, T)>,
        KC: for<'x> heed::BytesEncode<'x, EItem = (u8, u8, T, T)>,
    {
        match (left, right) {
            // If the request is an exact value we must go directly to the deepest level.
            (Included(l), Included(r)) if l == r && level > 0 => {
                return Self::explore_facet_levels::<T, KC>(rtxn, db, field_id, 0, left, right, output);
            },
            // lower TO upper when lower > upper must return no result
            (Included(l), Included(r)) if l > r => return Ok(()),
            (Included(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Included(r)) if l >= r => return Ok(()),
            (_, _) => (),
        }

        let mut left_found = None;
        let mut right_found = None;

        // We must create a custom iterator to be able to iterate over the
        // requested range as the range iterator cannot express some conditions.
        let left_bound = match left {
            Included(left) => Included((field_id, level, left, T::min_value())),
            Excluded(left) => Excluded((field_id, level, left, T::min_value())),
            Unbounded => Unbounded,
        };
        let right_bound = Included((field_id, level, T::max_value(), T::max_value()));
        // We also make sure that we don't decode the data before we are sure we must return it.
        let iter = db
            .remap_key_type::<KC>()
            .lazily_decode_data()
            .range(rtxn, &(left_bound, right_bound))?
            .take_while(|r| r.as_ref().map_or(true, |((.., r), _)| {
                match right {
                    Included(right) => *r <= right,
                    Excluded(right) => *r < right,
                    Unbounded => true,
                }
            }))
            .map(|r| r.and_then(|(key, lazy)| lazy.decode().map(|data| (key, data))));

        debug!("Iterating between {:?} and {:?} (level {})", left, right, level);

        for (i, result) in iter.enumerate() {
            let ((_fid, level, l, r), docids) = result?;
            debug!("{:?} to {:?} (level {}) found {} documents", l, r, level, docids.len());
            output.union_with(&docids);
            // We save the leftest and rightest bounds we actually found at this level.
            if i == 0 { left_found = Some(l); }
            right_found = Some(r);
        }

        // Can we go deeper?
        let deeper_level = match level.checked_sub(1) {
            Some(level) => level,
            None => return Ok(()),
        };

        // We must refine the left and right bounds of this range by retrieving the
        // missing part in a deeper level.
        match left_found.zip(right_found) {
            Some((left_found, right_found)) => {
                // If the bound is satisfied we avoid calling this function again.
                if !matches!(left, Included(l) if l == left_found) {
                    let sub_right = Excluded(left_found);
                    debug!("calling left with {:?} to {:?} (level {})",  left, sub_right, deeper_level);
                    Self::explore_facet_levels::<T, KC>(rtxn, db, field_id, deeper_level, left, sub_right, output)?;
                }
                if !matches!(right, Included(r) if r == right_found) {
                    let sub_left = Excluded(right_found);
                    debug!("calling right with {:?} to {:?} (level {})", sub_left, right, deeper_level);
                    Self::explore_facet_levels::<T, KC>(rtxn, db, field_id, deeper_level, sub_left, right, output)?;
                }
            },
            None => {
                // If we found nothing at this level it means that we must find
                // the same bounds but at a deeper, more precise level.
                Self::explore_facet_levels::<T, KC>(rtxn, db, field_id, deeper_level, left, right, output)?;
            },
        }

        Ok(())
    }

    fn evaluate_number_operator<'t, T: 't, KC>(
        rtxn: &'t heed::RoTxn,
        db: heed::Database<ByteSlice, CboRoaringBitmapCodec>,
        field_id: u8,
        operator: FacetNumberOperator<T>,
    ) -> anyhow::Result<RoaringBitmap>
    where
        T: Copy + PartialEq + PartialOrd + Bounded + Debug,
        KC: heed::BytesDecode<'t, DItem = (u8, u8, T, T)>,
        KC: for<'x> heed::BytesEncode<'x, EItem = (u8, u8, T, T)>,
    {
        // Make sure we always bound the ranges with the field id and the level,
        // as the facets values are all in the same database and prefixed by the
        // field id and the level.
        let (left, right) = match operator {
            GreaterThan(val)        => (Excluded(val),            Included(T::max_value())),
            GreaterThanOrEqual(val) => (Included(val),            Included(T::max_value())),
            LowerThan(val)          => (Included(T::min_value()), Excluded(val)),
            LowerThanOrEqual(val)   => (Included(T::min_value()), Included(val)),
            Equal(val)              => (Included(val),            Included(val)),
            Between(left, right)    => (Included(left),           Included(right)),
        };

        // Ask for the biggest value that can exist for this specific field, if it exists
        // that's fine if it don't, the value just before will be returned instead.
        let biggest_level = db
            .remap_types::<KC, DecodeIgnore>()
            .get_lower_than_or_equal_to(rtxn, &(field_id, u8::MAX, T::max_value(), T::max_value()))?
            .and_then(|((id, level, _, _), _)| if id == field_id { Some(level) } else { None });

        match biggest_level {
            Some(level) => {
                let mut output = RoaringBitmap::new();
                Self::explore_facet_levels::<T, KC>(rtxn, db, field_id, level, left, right, &mut output)?;
                Ok(output)
            },
            None => Ok(RoaringBitmap::new()),
        }
    }

    fn evaluate_string_operator(
        rtxn: &heed::RoTxn,
        db: heed::Database<FacetValueStringCodec, CboRoaringBitmapCodec>,
        field_id: u8,
        operator: &FacetStringOperator,
    ) -> anyhow::Result<RoaringBitmap>
    {
        match operator {
            FacetStringOperator::Equal(string) => {
                match db.get(rtxn, &(field_id, string))? {
                    Some(docids) => Ok(docids),
                    None => Ok(RoaringBitmap::new())
                }
            }
        }
    }

    pub fn evaluate(
        &self,
        rtxn: &heed::RoTxn,
        index: &Index,
    ) -> anyhow::Result<RoaringBitmap>
    {
        let db = index.facet_field_id_value_docids;
        match self {
            OperatorI64(fid, op) => {
                Self::evaluate_number_operator::<i64, FacetLevelValueI64Codec>(rtxn, db, *fid, *op)
            },
            OperatorF64(fid, op) => {
                Self::evaluate_number_operator::<f64, FacetLevelValueF64Codec>(rtxn, db, *fid, *op)
            },
            OperatorString(fid, op) => {
                let db = db.remap_key_type::<FacetValueStringCodec>();
                Self::evaluate_string_operator(rtxn, db, *fid, op)
            },
            Or(lhs, rhs) => {
                let lhs = lhs.evaluate(rtxn, index)?;
                let rhs = rhs.evaluate(rtxn, index)?;
                Ok(lhs | rhs)
            },
            And(lhs, rhs) => {
                let lhs = lhs.evaluate(rtxn, index)?;
                let rhs = rhs.evaluate(rtxn, index)?;
                Ok(lhs & rhs)
            },
            Not(op) => {
                // TODO is this right or is this wrong? because all documents ids are not faceted
                //      so doing that can return documents that are not faceted at all.
                let all_documents_ids = index.documents_ids(rtxn)?;
                let documents_ids = op.evaluate(rtxn, index)?;
                Ok(all_documents_ids - documents_ids)
            },
        }
    }
}
