use std::ops::Not;

use super::ErrorValidator;
use crate::context::{BidsContext, DatasetContext};

pub struct BFile;

#[async_trait::async_trait]
impl ErrorValidator for BFile {
    fn key(&self) -> &'static str {
        "BFile"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        if context.extension != ".bval" && context.extension != ".bvec" {
            return false;
        }
        if let Some(bfile_meta) = &context.bfile_meta {
            return bfile_meta.has_double_spaces || bfile_meta.has_non_numeric;
        }
        true
    }
}

pub struct BvecRowLength;

#[async_trait::async_trait]
impl ErrorValidator for BvecRowLength {
    fn key(&self) -> &'static str {
        "BvecRowLength"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        if context.extension != ".bvec" {
            return false;
        }
        if let Some(bvec) = &context.bfile_meta {
            return any_elements_not_equal(&bvec.row_lengths);
        }
        false
    }
}

fn any_elements_not_equal<T: PartialEq>(vec: &[T]) -> bool {
    vec.first()
        .is_none_or(|first| vec.iter().all(|x| x == first))
        .not()
}

pub struct MalformedBvec;

#[async_trait::async_trait]
impl ErrorValidator for MalformedBvec {
    fn key(&self) -> &'static str {
        "MalformedBvec"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        if context.extension != ".bvec" {
            return false;
        }
        context.bfile_meta.is_none()
    }
}

pub struct MalformedBval;

#[async_trait::async_trait]
impl ErrorValidator for MalformedBval {
    fn key(&self) -> &'static str {
        "MalformedBval"
    }

    async fn validate_file(&self, context: &BidsContext, _dataset: &DatasetContext) -> bool {
        if context.extension != ".bval" {
            return false;
        }
        context.bfile_meta.is_none()
    }
}
