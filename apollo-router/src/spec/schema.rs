//! GraphQL schema.

use std::collections::HashMap;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use apollo_compiler::hir;
use apollo_compiler::ApolloCompiler;
use apollo_compiler::AstDatabase;
use apollo_compiler::HirDatabase;
use http::Uri;
use itertools::Itertools;
use router_bridge::api_schema;
use sha2::Digest;
use sha2::Sha256;

use crate::error::ParseErrors;
use crate::error::SchemaError;
use crate::json_ext::Object;
use crate::json_ext::Value;
use crate::query_planner::OperationKind;
use crate::spec::query::parse_hir_value;
use crate::spec::FieldType;
use crate::Configuration;

/// A GraphQL schema.
#[derive(Debug)]
pub(crate) struct Schema {
    pub(crate) raw_sdl: Arc<String>,
    pub(crate) type_system: Arc<apollo_compiler::hir::TypeSystem>,
    subgraphs: HashMap<String, Uri>,
    pub(crate) object_types: HashMap<String, ObjectType>,
    pub(crate) interfaces: HashMap<String, Interface>,
    pub(crate) input_types: HashMap<String, InputObjectType>,
    pub(crate) custom_scalars: HashSet<String>,
    pub(crate) enums: HashMap<String, HashSet<String>>,
    api_schema: Option<Box<Schema>>,
    pub(crate) schema_id: Option<String>,
    root_operations: HashMap<OperationKind, String>,
}

fn make_api_schema(schema: &str) -> Result<String, SchemaError> {
    let s = api_schema::api_schema(schema)
        .map_err(|e| SchemaError::Api(e.to_string()))?
        .map_err(|e| SchemaError::Api(e.iter().filter_map(|e| e.message.as_ref()).join(", ")))?;
    Ok(format!("{s}\n"))
}

impl Schema {
    pub(crate) fn parse(s: &str, configuration: &Configuration) -> Result<Self, SchemaError> {
        let mut schema = parse(s, configuration)?;
        schema.api_schema = Some(Box::new(parse(&make_api_schema(s)?, configuration)?));
        return Ok(schema);

        fn parse(schema: &str, _configuration: &Configuration) -> Result<Schema, SchemaError> {
            let mut compiler = ApolloCompiler::new();
            compiler.add_type_system(
                include_str!("introspection_types.graphql"),
                "introspection_types.graphql",
            );
            let id = compiler.add_type_system(schema, "schema.graphql");

            let ast = compiler.db.ast(id);

            // Trace log recursion limit data
            let recursion_limit = ast.recursion_limit();
            tracing::trace!(?recursion_limit, "recursion limit data");

            // TODO: run full compiler-based validation instead?
            let errors = ast.errors().cloned().collect::<Vec<_>>();
            if !errors.is_empty() {
                let errors = ParseErrors {
                    raw_schema: schema.to_string(),
                    errors,
                };
                errors.print();
                return Err(SchemaError::Parse(errors));
            }

            fn as_string(value: &hir::Value) -> Option<&String> {
                if let hir::Value::String(string) = value {
                    Some(string)
                } else {
                    None
                }
            }

            let mut subgraphs = HashMap::new();
            // TODO: error if not found?
            if let Some(join_enum) = compiler.db.find_enum_by_name("join__Graph".into()) {
                for (name, url) in join_enum
                    .enum_values_definition()
                    .iter()
                    .filter_map(|value| {
                        let join_directive = value
                            .directives()
                            .iter()
                            .find(|directive| directive.name() == "join__graph")?;
                        let name = as_string(join_directive.argument_by_name("name")?)?;
                        let url = as_string(join_directive.argument_by_name("url")?)?;
                        Some((name, url))
                    })
                {
                    if url.is_empty() {
                        return Err(SchemaError::MissingSubgraphUrl(name.clone()));
                    }
                    let url = Uri::from_str(url)
                        .map_err(|err| SchemaError::UrlParse(name.clone(), err))?;
                    if subgraphs.insert(name.clone(), url).is_some() {
                        return Err(SchemaError::Api(format!(
                            "must not have several subgraphs with same name '{name}'"
                        )));
                    }
                }
            }

            let object_types: HashMap<_, _> = compiler
                .db
                .object_types()
                .iter()
                .map(|(name, def)| (name.clone(), (&**def).into()))
                .collect();

            let interfaces: HashMap<_, _> = compiler
                .db
                .interfaces()
                .iter()
                .map(|(name, def)| (name.clone(), (&**def).into()))
                .collect();

            let input_types: HashMap<_, _> = compiler
                .db
                .input_objects()
                .iter()
                .map(|(name, def)| (name.clone(), (&**def).into()))
                .collect();

            let enums = compiler
                .db
                .enums()
                .iter()
                .map(|(name, def)| {
                    let values = def
                        .enum_values_definition()
                        .iter()
                        .map(|value| value.enum_value().to_owned())
                        .collect();
                    (name.clone(), values)
                })
                .collect();

            let root_operations = compiler
                .db
                .schema()
                .root_operation_type_definition()
                .iter()
                .filter(|def| def.loc().is_some()) // exclude implict operations
                .map(|def| {
                    (
                        def.operation_ty().into(),
                        if let hir::Type::Named { name, .. } = def.named_type() {
                            name.clone()
                        } else {
                            // FIXME: hir::RootOperationTypeDefinition should contain
                            // the name directly, not a `Type` enum value which happens to always
                            // be the `Named` variant.
                            unreachable!()
                        },
                    )
                })
                .collect();

            let custom_scalars = compiler
                .db
                .scalars()
                .iter()
                .filter(|(_name, def)| !def.is_built_in())
                .map(|(name, _def)| name.clone())
                .collect();

            let mut hasher = Sha256::new();
            hasher.update(schema.as_bytes());
            let schema_id = Some(format!("{:x}", hasher.finalize()));

            Ok(Schema {
                raw_sdl: Arc::new(schema.into()),
                type_system: compiler.db.type_system(),
                subgraphs,
                object_types,
                interfaces,
                input_types,
                custom_scalars,
                enums,
                api_schema: None,
                schema_id,
                root_operations,
            })
        }
    }
}

impl Schema {
    /// Extracts a string containing the entire [`Schema`].
    pub(crate) fn as_string(&self) -> &Arc<String> {
        &self.raw_sdl
    }

    pub(crate) fn is_subtype(&self, abstract_type: &str, maybe_subtype: &str) -> bool {
        self.type_system
            .subtype_map
            .get(abstract_type)
            .map(|x| x.contains(maybe_subtype))
            .unwrap_or(false)
    }

    /// Return an iterator over subgraphs that yields the subgraph name and its URL.
    pub(crate) fn subgraphs(&self) -> impl Iterator<Item = (&String, &Uri)> {
        self.subgraphs.iter()
    }

    pub(crate) fn api_schema(&self) -> &Schema {
        match &self.api_schema {
            Some(schema) => schema,
            None => self,
        }
    }

    pub(crate) fn root_operation_name(&self, kind: OperationKind) -> &str {
        self.root_operations
            .get(&kind)
            .map(|s| s.as_str())
            .unwrap_or_else(|| kind.as_str())
    }
}

#[derive(Debug)]
pub(crate) struct InvalidObject;

#[derive(Debug, Clone)]
pub(crate) struct ObjectType {
    pub(crate) fields: HashMap<String, FieldType>,
}

#[derive(Debug, Clone)]
pub(crate) struct Interface {
    pub(crate) fields: HashMap<String, FieldType>,
}

macro_rules! implement_object_type_or_interface {
    ($name:ident => $hir_ty:ty $(,)?) => {
        impl From<&'_ $hir_ty> for $name {
            fn from(def: &'_ $hir_ty) -> Self {
                Self {
                    fields: def
                        .fields_definition()
                        .iter()
                        .chain(
                            def.extensions()
                                .iter()
                                .flat_map(|ext| ext.fields_definition()),
                        )
                        .map(|field| (field.name().to_owned(), field.ty().into()))
                        .collect(),
                }
            }
        }
    };
}

// Spec: https://spec.graphql.org/draft/#sec-Objects
// Spec: https://spec.graphql.org/draft/#sec-Object-Extensions
implement_object_type_or_interface!(
    ObjectType =>
    hir::ObjectTypeDefinition,
);
// Spec: https://spec.graphql.org/draft/#sec-Interfaces
// Spec: https://spec.graphql.org/draft/#sec-Interface-Extensions
implement_object_type_or_interface!(
    Interface =>
    hir::InterfaceTypeDefinition,
);

#[derive(Debug, Clone)]
pub(crate) struct InputObjectType {
    pub(crate) fields: HashMap<String, (FieldType, Option<Value>)>,
}

impl InputObjectType {
    pub(crate) fn validate_object(
        &self,
        object: &Object,
        schema: &Schema,
    ) -> Result<(), InvalidObject> {
        self.fields
            .iter()
            .try_for_each(|(name, (ty, default_value))| {
                let value = match object.get(name.as_str()) {
                    Some(&Value::Null) | None => default_value.as_ref().unwrap_or(&Value::Null),
                    Some(value) => value,
                };
                ty.validate_input_value(value, schema)
            })
            .map_err(|_| InvalidObject)
    }
}

impl From<&'_ hir::InputObjectTypeDefinition> for InputObjectType {
    fn from(def: &'_ hir::InputObjectTypeDefinition) -> Self {
        InputObjectType {
            fields: def
                .input_fields_definition()
                .iter()
                .chain(
                    def.extensions()
                        .iter()
                        .flat_map(|ext| ext.input_fields_definition()),
                )
                .map(|field| {
                    (
                        field.name().to_owned(),
                        (
                            field.ty().into(),
                            field.default_value().and_then(parse_hir_value),
                        ),
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_supergraph_boilerplate(content: &str) -> String {
        format!(
            "{}\n{}",
            r#"
        schema
            @core(feature: "https://specs.apollo.dev/core/v0.1")
            @core(feature: "https://specs.apollo.dev/join/v0.1") {
            query: Query
        }
        directive @core(feature: String!) repeatable on SCHEMA
        directive @join__graph(name: String!, url: String!) on ENUM_VALUE
        enum join__Graph {
            TEST @join__graph(name: "test", url: "http://localhost:4001/graphql")
        }

        "#,
            content
        )
    }

    #[test]
    fn is_subtype() {
        fn gen_schema_types(schema: &str) -> Schema {
            let base_schema = with_supergraph_boilerplate(
                r#"
            type Query {
              me: String
            }
            type Foo {
              me: String
            }
            type Bar {
              me: String
            }
            type Baz {
              me: String
            }
            
            union UnionType2 = Foo | Bar
            "#,
            );
            let schema = format!("{base_schema}\n{schema}");
            Schema::parse(&schema, &Default::default()).unwrap()
        }

        fn gen_schema_interfaces(schema: &str) -> Schema {
            let base_schema = with_supergraph_boilerplate(
                r#"
            type Query {
              me: String
            }
            interface Foo {
              me: String
            }
            interface Bar {
              me: String
            }
            interface Baz {
              me: String,
            }

            type ObjectType2 implements Foo & Bar { me: String }
            interface InterfaceType2 implements Foo & Bar { me: String }
            "#,
            );
            let schema = format!("{base_schema}\n{schema}");
            Schema::parse(&schema, &Default::default()).unwrap()
        }
        let schema = gen_schema_types("union UnionType = Foo | Bar | Baz");
        assert!(schema.is_subtype("UnionType", "Foo"));
        assert!(schema.is_subtype("UnionType", "Bar"));
        assert!(schema.is_subtype("UnionType", "Baz"));
        let schema =
            gen_schema_interfaces("type ObjectType implements Foo & Bar & Baz { me: String }");
        assert!(schema.is_subtype("Foo", "ObjectType"));
        assert!(schema.is_subtype("Bar", "ObjectType"));
        assert!(schema.is_subtype("Baz", "ObjectType"));
        let schema = gen_schema_interfaces(
            "interface InterfaceType implements Foo & Bar & Baz { me: String }",
        );
        assert!(schema.is_subtype("Foo", "InterfaceType"));
        assert!(schema.is_subtype("Bar", "InterfaceType"));
        assert!(schema.is_subtype("Baz", "InterfaceType"));
        let schema = gen_schema_types("extend union UnionType2 = Baz");
        assert!(schema.is_subtype("UnionType2", "Foo"));
        assert!(schema.is_subtype("UnionType2", "Bar"));
        assert!(schema.is_subtype("UnionType2", "Baz"));
        let schema =
            gen_schema_interfaces("extend type ObjectType2 implements Baz { me2: String }");
        assert!(schema.is_subtype("Foo", "ObjectType2"));
        assert!(schema.is_subtype("Bar", "ObjectType2"));
        assert!(schema.is_subtype("Baz", "ObjectType2"));
        let schema =
            gen_schema_interfaces("extend interface InterfaceType2 implements Baz { me2: String }");
        assert!(schema.is_subtype("Foo", "InterfaceType2"));
        assert!(schema.is_subtype("Bar", "InterfaceType2"));
        assert!(schema.is_subtype("Baz", "InterfaceType2"));
    }

    #[test]
    fn routing_urls() {
        let schema = r#"
        schema
          @core(feature: "https://specs.apollo.dev/core/v0.1"),
          @core(feature: "https://specs.apollo.dev/join/v0.1")
        {
          query: Query
        }
        type Query {
          me: String
        }
        directive @core(feature: String!) repeatable on SCHEMA
        directive @join__graph(name: String!, url: String!) on ENUM_VALUE

        enum join__Graph {
            ACCOUNTS @join__graph(name:"accounts" url: "http://localhost:4001/graphql")
            INVENTORY
              @join__graph(name: "inventory", url: "http://localhost:4004/graphql")
            PRODUCTS
            @join__graph(name: "products" url: "http://localhost:4003/graphql")
            REVIEWS @join__graph(name: "reviews" url: "http://localhost:4002/graphql")
        }"#;
        let schema = Schema::parse(schema, &Default::default()).unwrap();

        assert_eq!(schema.subgraphs.len(), 4);
        assert_eq!(
            schema
                .subgraphs
                .get("accounts")
                .map(|s| s.to_string())
                .as_deref(),
            Some("http://localhost:4001/graphql"),
            "Incorrect url for accounts"
        );

        assert_eq!(
            schema
                .subgraphs
                .get("inventory")
                .map(|s| s.to_string())
                .as_deref(),
            Some("http://localhost:4004/graphql"),
            "Incorrect url for inventory"
        );

        assert_eq!(
            schema
                .subgraphs
                .get("products")
                .map(|s| s.to_string())
                .as_deref(),
            Some("http://localhost:4003/graphql"),
            "Incorrect url for products"
        );

        assert_eq!(
            schema
                .subgraphs
                .get("reviews")
                .map(|s| s.to_string())
                .as_deref(),
            Some("http://localhost:4002/graphql"),
            "Incorrect url for reviews"
        );

        assert_eq!(schema.subgraphs.get("test"), None);
    }

    #[test]
    fn api_schema() {
        let schema = include_str!("../testdata/contract_schema.graphql");
        let schema = Schema::parse(schema, &Default::default()).unwrap();
        assert!(schema.object_types["Product"]
            .fields
            .get("inStock")
            .is_some());
        assert!(schema.api_schema.unwrap().object_types["Product"]
            .fields
            .get("inStock")
            .is_none());
    }

    #[test]
    fn schema_id() {
        #[cfg(not(windows))]
        {
            let schema = include_str!("../testdata/starstuff@current.graphql");
            let schema = Schema::parse(schema, &Default::default()).unwrap();

            assert_eq!(
                schema.schema_id,
                Some(
                    "8e2021d131b23684671c3b85f82dfca836908c6a541bbd5c3772c66e7f8429d8".to_string()
                )
            );

            assert_eq!(
                schema.api_schema().schema_id,
                Some(
                    "ba573b479c8b3fa273f439b26b9eda700152341d897f18090d52cd073b15f909".to_string()
                )
            );
        }
    }

    // test for https://github.com/apollographql/federation/pull/1769
    #[test]
    fn inaccessible_on_non_core() {
        let schema = include_str!("../testdata/inaccessible_on_non_core.graphql");
        match Schema::parse(schema, &Default::default()) {
            Err(SchemaError::Api(s)) => {
                assert_eq!(
                    s,
                    r#"The supergraph schema failed to produce a valid API schema. Caused by:
Input field "InputObject.privateField" is @inaccessible but is used in the default value of "@foo(someArg:)", which is in the API schema.

GraphQL request:42:1
41 |
42 | input InputObject {
   | ^
43 |   someField: String"#
                );
            }
            other => panic!("unexpected schema result: {other:?}"),
        };
    }

    // https://github.com/apollographql/router/issues/2269
    #[test]
    fn unclosed_brace_error_does_not_panic() {
        let schema = "schema {";
        let result = Schema::parse(schema, &Default::default());
        assert!(result.is_err());
    }
}
