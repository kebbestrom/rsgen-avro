use std::collections::{HashMap, HashSet};

use avro_rs::schema::{Name, RecordField};
use avro_rs::Schema;
use by_address::ByAddress;
use failure::{Error, SyncFailure};
use heck::{CamelCase, SnakeCase};
use serde_json::Value;
use tera::{Context, Tera};

pub const RECORD_TERA: &str = "record.tera";
pub const RECORD_TEMPLATE: &str = "
#[serde(default)]
#[derive(Debug, Deserialize, Serialize)]
pub struct {{ name }} {
    {%- for f, type in fields %}
    {%- if f != originals[f] %}
    #[serde(rename = \"{{ originals[f] }}\")]
    {%- endif %}
    pub {{ f }}: {{ type }},
    {%- endfor %}
}

impl Default for {{ name }} {
    fn default() -> {{ name }} {
        {{ name }} {
            {%- for f, value in defaults %}
            {{ f }}: {{ value }},
            {%- endfor %}
        }
    }
}
";

pub const ENUM_TERA: &str = "enum.tera";
pub const ENUM_TEMPLATE: &str = "
#[derive(Debug, Deserialize, Serialize)]
pub enum {{ name }} {
    {%- for s, o in symbols %}
    {%- if s != o %}
    #[serde(rename = \"{{ o }}\")]
    {%- endif %}
    {{ s }},
    {%- endfor %}
}
";

pub const FIXED_TERA: &str = "fixed.tera";
pub const FIXED_TEMPLATE: &str = "
pub type {{ name }} = [u8; {{ size }}];
";

lazy_static! {
    static ref RESERVED: HashSet<String> = {
        let s: HashSet<_> = vec![
            "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
            "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
            "return", "Self", "self", "static", "struct", "super", "trait", "true", "type",
            "unsage", "use", "where", "while", "abstract", "alignof", "become", "box", "do",
            "final", "macro", "offsetof", "override", "priv", "proc", "pure", "sizeof", "typeof",
            "unsized", "virtual", "yields",
        ].iter()
        .map(|s| s.to_string())
        .collect();
        s
    };
}

fn sanitize(mut s: String) -> String {
    if RESERVED.contains(&s) {
        s.push_str("_");
        s
    } else {
        s
    }
}

/// Describes errors happened while templating Rust code.
#[derive(Fail, Debug)]
#[fail(display = "Template failure: {}", _0)]
pub struct TemplateError(String);

impl TemplateError {
    pub fn new<S>(msg: S) -> TemplateError
    where
        S: Into<String>,
    {
        TemplateError(msg.into())
    }
}

macro_rules! err(
    ($($arg:tt)*) => (Err(TemplateError::new(format!($($arg)*))))
);

// https://github.com/rust-lang-nursery/failure/issues/109
trait ResultExt<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static;
}

// https://github.com/rust-lang-nursery/failure/issues/109
impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static,
    {
        self.map_err(SyncFailure::new)
    }
}

#[derive(Debug)]
pub struct GenState<'a>(HashMap<ByAddress<&'a Schema>, String>);

impl<'a> GenState<'a> {
    pub fn new() -> GenState<'a> {
        GenState(HashMap::new())
    }

    pub fn put_type<'b: 'a>(&mut self, schema: &'b Schema, t: String) {
        self.0.insert(ByAddress(schema), t);
    }

    pub fn get_type(&self, schema: &'a Schema) -> Option<&String> {
        self.0.get(&ByAddress(schema))
    }
}

pub struct Templater {
    tera: Tera,
}

impl Templater {
    pub fn new() -> Result<Templater, Error> {
        let mut tera = Tera::new("/dev/null/*").sync()?;
        tera.add_raw_template(RECORD_TERA, RECORD_TEMPLATE).sync()?;
        tera.add_raw_template(ENUM_TERA, ENUM_TEMPLATE).sync()?;
        tera.add_raw_template(FIXED_TERA, FIXED_TEMPLATE).sync()?;
        Ok(Templater { tera })
    }

    pub fn str_fixed(&self, schema: &Schema) -> Result<String, Error> {
        if let Schema::Fixed {
            name: Name { name, .. },
            size,
        } = schema
        {
            let mut ctx = Context::new();
            ctx.insert("name", &sanitize(name.to_camel_case()));
            ctx.insert("size", size);
            Ok(self.tera.render(FIXED_TERA, &ctx).sync()?)
        } else {
            err!("Requires Schema::Fixed, found {:?}", schema)?
        }
    }

    pub fn str_enum(&self, schema: &Schema) -> Result<String, Error> {
        if let Schema::Enum {
            name: Name { name, .. },
            symbols,
            ..
        } = schema
        {
            if symbols.len() == 0 {
                err!("No symbol for emum: {:?}", name)?
            }
            let mut ctx = Context::new();
            ctx.insert("name", &sanitize(name.to_camel_case()));
            let s: HashMap<_, _> = symbols
                .iter()
                .map(|s| (sanitize(s.to_camel_case()), s))
                .collect();
            ctx.insert("symbols", &s);
            Ok(self.tera.render(ENUM_TERA, &ctx).sync()?)
        } else {
            err!("Requires Schema::Enum, found {:?}", schema)?
        }
    }

    pub fn str_record(&self, schema: &Schema, gen_state: &GenState) -> Result<String, Error> {
        if let Schema::Record {
            name: Name { name, .. },
            fields,
            ..
        } = schema
        {
            let mut ctx = Context::new();
            ctx.insert("name", &name.to_camel_case());

            let mut f = HashMap::new(); // field name -> field type
            let mut o = HashMap::new(); // field name -> original name
            let mut d = HashMap::new(); // field name -> default value
            for RecordField {
                schema,
                name,
                default,
                ..
            } in fields
            {
                let name_std = sanitize(name.to_snake_case());
                o.insert(name_std.clone(), name);

                match schema {
                    Schema::Boolean => {
                        let default = match default {
                            Some(Value::Bool(b)) => b.to_string(),
                            None => bool::default().to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "bool".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Int => {
                        let default = match default {
                            Some(Value::Number(n)) if n.is_i64() => n.to_string(),
                            None => i32::default().to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "i32".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Long => {
                        let default = match default {
                            Some(Value::Number(n)) if n.is_i64() => n.to_string(),
                            None => i64::default().to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "i64".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Float => {
                        let default = match default {
                            Some(Value::Number(n)) if n.is_f64() => n.to_string(),
                            None => f32::default().to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "f32".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Double => {
                        let default = match default {
                            Some(Value::Number(n)) if n.is_f64() => n.to_string(),
                            None => f64::default().to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "f64".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Bytes => {
                        let default = match default {
                            Some(Value::String(s)) => {
                                let bytes = s.clone().into_bytes();
                                format!("vec!{:?}", bytes)
                            }
                            None => "vec![]".to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "Vec<u8>".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::String => {
                        let default = match default {
                            Some(Value::String(s)) => format!("\"{}\".to_owned()", s),
                            None => "String::default()".to_string(),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), "String".to_string());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Fixed {
                        name: Name { name: f_name, .. },
                        size,
                    } => {
                        let f_name = sanitize(f_name.to_camel_case());
                        let default = match default {
                            Some(Value::String(s)) => {
                                let bytes: Vec<u8> = s.clone().into_bytes();
                                if bytes.len() != *size {
                                    err!("Invalid default: {:?}", bytes)?
                                }
                                format!("{:?}", bytes)
                            }
                            None => format!("{}::default()", f_name),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), f_name.clone());
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Array(inner) => match &**inner {
                        Schema::Null => err!("Invalid use of Schema::Null")?,
                        _ => {
                            let type_str = array_type(&**inner, &*gen_state)?;
                            let default_str = array_default(&**inner, default)?;
                            f.insert(name_std.clone(), type_str);
                            d.insert(name_std.clone(), default_str);
                        }
                    },

                    Schema::Map(inner) => match &**inner {
                        Schema::Null => err!("Invalid use of Schema::Null")?,
                        _ => {
                            let type_str = map_type(&**inner, &*gen_state)?;
                            let default_str = map_default(&**inner, default)?;
                            f.insert(name_std.clone(), type_str);
                            d.insert(name_std.clone(), default_str);
                        }
                    },

                    Schema::Record {
                        name: Name { name: r_name, .. },
                        ..
                    } => {
                        let r_name = sanitize(r_name.to_camel_case());
                        f.insert(name_std.clone(), r_name.clone());
                        d.insert(name_std.clone(), format!("{}::default()", r_name));
                    }

                    Schema::Enum {
                        name: Name { name: e_name, .. },
                        symbols,
                        ..
                    } => {
                        let e_name = sanitize(e_name.to_camel_case());
                        let default = match default {
                            Some(Value::String(s)) => s.clone(),
                            None if !symbols.is_empty() => sanitize(symbols[0].to_camel_case()),
                            _ => err!("Invalid default: {:?}", default)?,
                        };
                        f.insert(name_std.clone(), e_name);
                        d.insert(name_std.clone(), default);
                    }

                    Schema::Union(union) => {
                        if let [Schema::Null, inner] = union.variants() {
                            let type_str = option_type(inner, &*gen_state)?;
                            let default_str = option_default(inner, default)?;
                            f.insert(name_std.clone(), type_str);
                            d.insert(name_std.clone(), default_str);
                        } else {
                            err!("Unsupported Schema:::Union {:?}", union.variants())?
                        }
                    }

                    Schema::Null => err!("Invalid use of Schema::Null")?,
                };
            }
            ctx.insert("fields", &f);
            ctx.insert("originals", &o);
            ctx.insert("defaults", &d);

            Ok(self.tera.render(RECORD_TERA, &ctx).sync()?)
        } else {
            err!("Requires Schema::Record, found {:?}", schema)?
        }
    }
}

pub fn array_type(inner: &Schema, gen_state: &GenState) -> Result<String, Error> {
    let type_str = match inner {
        Schema::Boolean => "Vec<bool>".to_string(),
        Schema::Int => "Vec<i32>".to_string(),
        Schema::Long => "Vec<i64>".to_string(),
        Schema::Float => "Vec<f32>".to_string(),
        Schema::Double => "Vec<f64>".to_string(),
        Schema::Bytes => "Vec<Vec<u8>>".to_string(),
        Schema::String => "Vec<String>".to_string(),

        Schema::Fixed {
            name: Name { name: f_name, .. },
            ..
        } => {
            let f_name = sanitize(f_name.to_camel_case());
            format!("Vec<{}>", f_name)
        }

        Schema::Array(..) | Schema::Map(..) | Schema::Union(..) => {
            let nested_type = gen_state.get_type(inner).ok_or_else(|| {
                TemplateError(format!(
                    "Didn't find schema {:?} in state {:?}",
                    inner, &gen_state
                ))
            })?;
            format!("Vec<{}>", nested_type)
        }

        Schema::Record {
            name: Name { name, .. },
            ..
        }
        | Schema::Enum {
            name: Name { name, .. },
            ..
        } => format!("Vec<{}>", &sanitize(name.to_camel_case())),

        Schema::Null => err!("Invalid use of Schema::Null")?,
    };
    Ok(type_str)
}

fn array_default(inner: &Schema, default: &Option<Value>) -> Result<String, TemplateError> {
    let to_default_str: Box<Fn(&Value) -> Result<String, TemplateError>> = match inner {
        Schema::Null => err!("Invalid use of Schema::Null")?,

        Schema::Boolean => Box::new(|v: &Value| match v {
            Value::Bool(b) => Ok(b.to_string()),
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Int => Box::new(|v: &Value| match v {
            Value::Number(n) if n.is_i64() => Ok(n.to_string()),
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Long => Box::new(|v: &Value| match v {
            Value::Number(n) if n.is_i64() => Ok(n.to_string()),
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Float | Schema::Double => Box::new(|v: &Value| match v {
            Value::Number(n) if n.is_f64() => Ok(n.to_string()),
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Bytes => Box::new(|v: &Value| match v {
            Value::String(s) => {
                let bytes = s.clone().into_bytes();
                Ok(format!("vec!{:?}", bytes))
            }
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::String => Box::new(|v: &Value| match v {
            Value::String(s) => Ok(s.clone()),
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Fixed { size, .. } => Box::new(move |v: &Value| match v {
            Value::String(s) => {
                let bytes: Vec<u8> = s.clone().into_bytes();
                if bytes.len() == *size {
                    Ok(format!("{:?}", bytes))
                } else {
                    err!("Invalid defaults: {:?}", bytes)
                }
            }
            _ => err!("Invalid defaults: {:?}", v),
        }),

        Schema::Array(s) => Box::new(move |v: &Value| Ok(array_default(s, &Some(v.clone()))?)),

        Schema::Map(s) => Box::new(move |v: &Value| Ok(map_default(s, &Some(v.clone()))?)),

        Schema::Enum { symbols, .. } => {
            let valids: HashSet<_> = symbols
                .iter()
                .map(|s| sanitize(s.to_camel_case()))
                .collect();
            Box::new(move |v: &Value| match v {
                Value::String(s) => {
                    let s = sanitize(s.to_camel_case());
                    if valids.contains(&s) {
                        Ok(s)
                    } else {
                        err!("Invalid defaults: {:?}", s)
                    }
                }
                _ => err!("Invalid defaults: {:?}", v),
            })
        }

        Schema::Union(..) => Box::new(|_| Ok("None".to_string())),

        Schema::Record {
            name: Name { name, .. },
            ..
        } => Box::new(move |_| Ok(format!("{}::default()", sanitize(name.to_camel_case())))),
    };

    let default_str = if let Some(Value::Array(vals)) = default {
        let vals = vals
            .iter()
            .map(&*to_default_str)
            .collect::<Result<Vec<String>, TemplateError>>()?
            .as_slice()
            .join(", ");
        format!("vec![{}]", vals)
    } else {
        "vec![]".to_string()
    };
    Ok(default_str)
}

fn map_of(t: &str) -> String {
    format!("::std::collections::HashMap<String, {}>", t)
}

pub fn map_type(inner: &Schema, gen_state: &GenState) -> Result<String, Error> {
    let type_str = match inner {
        Schema::Boolean => map_of("bool"),
        Schema::Int => map_of("i32"),
        Schema::Long => map_of("i64"),
        Schema::Float => map_of("f32"),
        Schema::Double => map_of("f64"),
        Schema::Bytes => map_of("Vec<u8>"),
        Schema::String => map_of("String"),

        Schema::Fixed {
            name: Name { name: f_name, .. },
            ..
        } => {
            let f_name = sanitize(f_name.to_camel_case());
            map_of(&f_name)
        }

        Schema::Array(..) | Schema::Map(..) | Schema::Union(..) => {
            let nested_type = gen_state.get_type(inner).ok_or_else(|| {
                TemplateError(format!(
                    "Didn't find schema {:?} in state {:?}",
                    inner, &gen_state
                ))
            })?;
            map_of(nested_type)
        }

        Schema::Record {
            name: Name { name, .. },
            ..
        }
        | Schema::Enum {
            name: Name { name, .. },
            ..
        } => map_of(&sanitize(name.to_camel_case())),

        Schema::Null => err!("Invalid use of Schema::Null")?,
    };
    Ok(type_str)
}

fn map_default(_: &Schema, _: &Option<Value>) -> Result<String, TemplateError> {
    let default_str = "::std::collections::HashMap::new()".to_string();
    Ok(default_str)
}

pub fn option_type(inner: &Schema, gen_state: &GenState) -> Result<String, Error> {
    let type_str = match inner {
        Schema::Boolean => "Option<bool>".to_string(),
        Schema::Int => "Option<i32>".to_string(),
        Schema::Long => "Option<i64>".to_string(),
        Schema::Float => "Option<f32>".to_string(),
        Schema::Double => "Option<f64>".to_string(),
        Schema::Bytes => "Option<Vec<u8>>".to_string(),
        Schema::String => "Option<String>".to_string(),

        Schema::Fixed {
            name: Name { name: f_name, .. },
            ..
        } => {
            let f_name = sanitize(f_name.to_camel_case());
            format!("Option<{}>", f_name)
        }

        Schema::Array(..) | Schema::Map(..) | Schema::Union(..) => {
            let nested_type = gen_state.get_type(inner).ok_or_else(|| {
                TemplateError(format!(
                    "Didn't find schema {:?} in state {:?}",
                    inner, &gen_state
                ))
            })?;
            format!("Option<{}>", nested_type)
        }

        Schema::Record {
            name: Name { name, .. },
            ..
        }
        | Schema::Enum {
            name: Name { name, .. },
            ..
        } => format!("Option<{}>", &sanitize(name.to_camel_case())),

        Schema::Null => err!("Invalid use of Schema::Null")?,
    };
    Ok(type_str)
}

fn option_default(_: &Schema, default: &Option<Value>) -> Result<String, Error> {
    let default_str = match default {
        None => "None".to_string(),
        Some(Value::String(s)) if s == "null" => "None".to_string(),
        Some(Value::String(s)) if s != "null" => err!("Invalid default: {:?}", s)?,
        _ => err!("Invalid default: {:?}", default)?,
    };
    Ok(default_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tera() {
        let raw_schema = r#"
        {"namespace": "example.avro",
         "type": "record",
         "name": "User",
         "fields": [
             {"name": "as", "type": "string"},
             {"name": "favoriteNumber",  "type": "int", "default": 7},
             {"name": "likes_pizza", "type": "boolean", "default": false},
             {"name": "b", "type": "bytes", "default": "\u00FF"},
             {"name": "a-bool", "type": {"type": "array", "items": "boolean"}, "default": [true, false]},
             {"name": "a-i32", "type": {"type": "array", "items": "int"}, "default": [12, -1]},
             {"name": "m-f64", "type": {"type": "map", "values": "double"}}
         ]
        }"#;

        let templater = Templater::new().unwrap();
        let schema = Schema::parse_str(&raw_schema).unwrap();
        let gs = GenState::new();
        let res = templater.str_record(&schema, &gs).unwrap();
        println!("{}", res);
    }

    #[test]
    fn tero() {
        let raw_schema = r#"
        {"type": "enum",
         "name": "Colors",
         "symbols": ["GREEN", "BLUE"]
        }"#;

        let templater = Templater::new().unwrap();
        let schema = Schema::parse_str(&raw_schema).unwrap();
        let res = templater.str_enum(&schema).unwrap();
        println!("{}", res);
    }

    #[test]
    fn teri() {
        let raw_schema = r#"
        {"type": "fixed",
         "name": "Md5",
         "size": 2
        }"#;

        let templater = Templater::new().unwrap();
        let schema = Schema::parse_str(&raw_schema).unwrap();
        let res = templater.str_fixed(&schema).unwrap();
        println!("{}", res);
    }
}
