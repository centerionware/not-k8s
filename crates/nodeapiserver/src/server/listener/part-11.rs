
fn json_response_with_content_type(status: StatusCode, value: &serde_json::Value, content_type: &str) -> Response<BoxedBody> {
    include!("body-15-1.rs");
    include!("body-15-2.rs");
}
