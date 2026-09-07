use serde::{
    Deserialize,
    Serialize,
};
use sloper_extension::{
    Schema,
    Source,
};

#[derive(Debug, Deserialize, Serialize, Schema)]
struct Document {
    #[schema(media_types = ["application/pdf"], max_bytes = 10_485_760)]
    document: Option<Source>,
}

#[test]
fn source_constraints_preserve_nullable_source_shape() {
    let schema: serde_json::Value = serde_json::from_str(Document::JSON).unwrap();
    let document = &schema["properties"]["document"];
    assert_eq!(document["type"], serde_json::json!(["string", "null"]));
    assert_eq!(document["format"], "source");
    assert_eq!(document["mediaTypes"], serde_json::json!(["application/pdf"]));
    assert_eq!(document["maxBytes"], 10_485_760);
}
