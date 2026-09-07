use sloper_extension::Schema;

#[derive(Schema)]
struct Value {
    #[schema(format = "hostname")]
    field: String,
}

#[derive(Schema)]
struct ManagedSource {
    #[schema(format = "source")]
    field: String,
}

fn main() {}
