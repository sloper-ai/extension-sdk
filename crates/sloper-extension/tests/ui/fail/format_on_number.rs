use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[schema(format = "date")]
    field: u32,
}

fn main() { let _ = Value::JSON; }
