//! The review submission (Spec C §4.4). The types, limits and renderer
//! live in `balerix_plugin_common::review` (Spec K-7); this keeps the
//! route's name for the request body.

pub use balerix_plugin_common::review::{
    Comment, MAX_BODY_BYTES, MAX_COMMENTS, MAX_MESSAGE_BYTES, MAX_REF_BYTES, MAX_TEXT_BYTES, Side,
    render_message, validate,
};

/// What `POST /agents/{id}/review` takes.
pub type ReviewBody = balerix_plugin_common::review::Review;
