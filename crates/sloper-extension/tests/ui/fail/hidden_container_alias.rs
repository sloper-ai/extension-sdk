use sloper_extension::Schema;

type Names = Vec<String>;

#[derive(Schema)]
struct Value { names: Names }

fn main() { let _ = Value::JSON; }
