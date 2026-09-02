
fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxedBody> {
    json_response_with_content_type(status, value, "application/json")
}
