use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[serde(skip_serializing)]
    field: String,
}

fn main() {}
