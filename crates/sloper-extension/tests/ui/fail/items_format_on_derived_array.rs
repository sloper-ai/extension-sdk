use sloper_extension::Schema;

#[derive(Schema)]
struct Values(Vec<String>);

#[derive(Schema)]
struct Value {
    #[schema(items(format = "email"))]
    field: Values,
}

fn main() {}
