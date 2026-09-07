use sloper_extension::Schema;

#[derive(Schema)]
struct Unsupported {
    #[schema(items(format = "hostname"))]
    field: Vec<String>,
}

#[derive(Schema)]
struct Duplicate {
    #[schema(items(format = "email", format = "uri"))]
    field: Vec<String>,
}

#[derive(Schema)]
struct Empty {
    #[schema(items())]
    field: Vec<String>,
}

fn main() {}
