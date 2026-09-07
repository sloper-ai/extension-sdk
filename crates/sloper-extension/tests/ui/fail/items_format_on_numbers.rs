use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[schema(items(format = "email"))]
    field: Vec<u32>,
}

fn main() { let _ = Value::JSON; }
