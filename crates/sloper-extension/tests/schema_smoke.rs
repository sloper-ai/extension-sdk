use std::collections::BTreeMap;

use serde::{
    Deserialize,
    Serialize,
};
use sloper_extension::Schema;

#[derive(Debug, Deserialize, Serialize, Schema)]
#[serde(rename_all = "camelCase")]
struct Child {
    value: String,
}

#[derive(Debug, Deserialize, Serialize, Schema)]
struct Sample {
    #[schema(max_length = 20)]
    name: String,
    child: Child,
    optional: Option<u32>,
    values: Vec<String>,
    fixed: [i32; 2],
    map: BTreeMap<String, Child>,
}

#[test]
fn schema_is_deterministic_and_declared() {
    let json: serde_json::Value = serde_json::from_str(Sample::JSON).unwrap();
    assert_eq!(json["additionalProperties"], false);
    assert_eq!(json["properties"]["name"]["maxLength"], 20);
    assert_eq!(json["properties"]["fixed"]["minItems"], 2);
    assert_eq!(
        json["properties"]["optional"]["type"],
        serde_json::json!(["integer", "null"])
    );
    assert_eq!(
        json["required"],
        serde_json::json!(["name", "child", "values", "fixed", "map"])
    );
    assert!(Sample::JSON.find("\"name\"").unwrap() < Sample::JSON.find("\"child\"").unwrap());
}

#[derive(Debug, Deserialize, Serialize, Schema)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ready,
    NeedsReview,
}

#[derive(Debug, Deserialize, Serialize, Schema)]
struct OpenItem {
    status: Status,
    #[serde(default)]
    count: u32,
    #[serde(flatten)]
    fields: sloper_extension::Fields,
    #[serde(skip)]
    skipped: String,
}

#[test]
fn serde_renames_defaults_skips_and_open_fields_match() {
    let schema: serde_json::Value = serde_json::from_str(OpenItem::JSON).unwrap();
    let item = OpenItem {
        status: Status::NeedsReview,
        count: 0,
        fields: [("extra".into(), true.into())].into(),
        skipped: String::new(),
    };
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(
        schema["properties"]["status"]["enum"],
        serde_json::json!(["ready", "needs_review"])
    );
    assert_eq!(schema["required"], serde_json::json!(["status"]));
    assert_eq!(schema["additionalProperties"], true);
    assert_eq!(value["status"], "needs_review");
    assert_eq!(value["extra"], true);
    assert!(schema["properties"].get("skipped").is_none());
    assert!(!value.as_object().unwrap().contains_key("skipped"));
    assert_eq!(item.skipped, "");
}

#[derive(Serialize, Schema)]
#[schema(minimum = 2, maximum = 30)]
struct Count(u32);

#[derive(Serialize, Schema)]
struct Refined {
    #[schema(minimum = -3)]
    unsigned: u32,
    #[schema(minimum = 5, maximum = 20)]
    count: Count,
}

#[test]
fn constraints_refine_without_duplicate_keys_or_widening() {
    let value = Refined {
        unsigned: 0,
        count: Count(12),
    };
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        serde_json::json!({"unsigned": 0, "count": 12})
    );
    let schema: serde_json::Value = serde_json::from_str(Refined::JSON).unwrap();
    assert_eq!(schema["properties"]["unsigned"]["minimum"], 0);
    assert_eq!(schema["properties"]["count"]["minimum"], 5);
    assert_eq!(schema["properties"]["count"]["maximum"], 20);
    assert_eq!(Refined::JSON.matches("\"minimum\"").count(), 2);
}
