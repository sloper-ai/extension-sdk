use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[schema(max_bytes = 100)]
    field: String,
}

fn main() { let _ = Value::JSON; }
