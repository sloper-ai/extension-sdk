use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[schema(items(format = "email"))]
    field: String,
}

fn main() {}
