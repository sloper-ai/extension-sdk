use sloper_extension::{Schema, Source};

#[derive(Schema)]
struct Value {
    #[schema(format = "uri")]
    field: Source,
}

fn main() { let _ = Value::JSON; }
