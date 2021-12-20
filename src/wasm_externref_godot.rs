use std::io::Write;

use gdnative::prelude::*;
use wasmtime::{Caller, ExternRef, Func, Linker, Trap};

/// Godot module name
pub const GODOT_MODULE: &str = "godot";

#[inline]
pub fn variant_to_externref(object: Variant) -> Option<ExternRef> {
    if object.is_nil() {
        None
    } else {
        Some(ExternRef::new(object))
    }
}

#[inline]
pub fn externref_to_variant(ext: Option<ExternRef>) -> Result<Variant, Trap> {
    ext.map_or_else(
        || Ok(Variant::new()),
        |v| {
            v.data()
                .downcast_ref::<Variant>()
                .cloned()
                .ok_or_else(|| Trap::new("External reference is not a Godot variant"))
        },
    )
}

#[inline(always)]
fn externref_to_variant_nonnull(ext: Option<ExternRef>) -> Result<Variant, Trap> {
    ext.ok_or_else(|| Trap::new("Null value")).and_then(|v| {
        v.data()
            .downcast_ref::<Variant>()
            .cloned()
            .ok_or_else(|| Trap::new("External reference is not a Godot variant"))
    })
}

#[inline(always)]
fn externref_to_object<T: FromVariant>(ext: Option<ExternRef>) -> Result<T, Trap> {
    externref_to_variant_nonnull(ext)
        .and_then(|v| T::from_variant(&v).map_err(|e| Trap::from(Box::new(e) as Box<_>)))
}

macro_rules! variant_convert {
    ($l:ident, $t:ty, ($from:literal, $to:literal)) => {{
        $l.func_wrap(GODOT_MODULE, $from, |v: $t| {
            variant_to_externref(v.to_variant())
        })?;

        $l.func_wrap(GODOT_MODULE, $to, externref_to_object::<$t>)?;
    }};
    ($l:ident, $t:ty => ($($v:ident : $t2:ty),*), ($from:literal, $to:literal)) => {{
        $l.func_wrap(GODOT_MODULE, $from, |$($v: $t2),*| {
            variant_to_externref(<$t as From<($($t2),*)>>::from(($($v),*)).to_variant())
        })?;

        $l.func_wrap(GODOT_MODULE, $to, |v| -> Result<($($t2),*), Trap> {
            externref_to_object::<$t>(v).map(|v| v.into())
        })?;
    }};
    ($l:ident, $t:ty => $t2:ty, ($from:literal, $to:literal)) => {{
        $l.func_wrap(GODOT_MODULE, $from, |v: $t2| {
            variant_to_externref(<$t as From<$t2>>::from(v).to_variant())
        })?;

        $l.func_wrap(GODOT_MODULE, $to, |v| -> Result<$t2, Trap> {
            externref_to_object::<$t>(v).map(|v| v.into())
        })?;
    }};
}

macro_rules! variant_typecheck {
    ($l:ident, $t:pat, $is:literal) => {
        $l.func_wrap(GODOT_MODULE, $is, |v: Option<ExternRef>| {
            v.and_then(|v| {
                v.data()
                    .downcast_ref::<Variant>()
                    .map(|v| matches!(v.get_type(), $t))
            })
            .unwrap_or(false) as i32
        })?
    };
}

macro_rules! object_new {
    ($l:ident, $t:ty, $new:literal) => {
        $l.func_wrap(GODOT_MODULE, $new, || {
            variant_to_externref(<$t>::new().owned_to_variant())
        })?
    };
}

macro_rules! object_call {
    ($l:ident, fn $dup:literal ( $v:ident : $t:ty $(, $a:ident $(: $ta:ty)?)* ) $b:block) => {
        $l.func_wrap(GODOT_MODULE, $dup, |$($a $(: $ta)?,)* $v| {
            let $v = externref_to_object::<$t>($v)?;
            Ok($b)
        })?
    };
    ($l:ident, fn $dup:literal ( $ctx:pat, $v:ident : $t:ty $(, $a:ident $(: $ta:ty)?)* ) $b:block) => {
        $l.func_wrap(GODOT_MODULE, $dup, |$ctx: Caller<_>, $($a $(: $ta)?,)* $v| {
            let $v = externref_to_object::<$t>($v)?;
            Ok($b)
        })?
    };
}

/// Register godot module
pub fn register_godot_externref<T>(linker: &mut Linker<T>) -> anyhow::Result<()> {
    linker.func_wrap(GODOT_MODULE, "var.is_var", |v: Option<ExternRef>| {
        v.map(|v| v.data().downcast_ref::<Variant>().is_some())
            .unwrap_or(false) as i32
    })?;

    variant_typecheck!(linker, VariantType::I64, "var.is_int");
    variant_typecheck!(linker, VariantType::F64, "var.is_float");
    variant_typecheck!(linker, VariantType::Bool, "var.is_bool");
    variant_typecheck!(linker, VariantType::Vector2, "var.is_vec2");
    variant_typecheck!(linker, VariantType::Vector3, "var.is_vec3");
    variant_typecheck!(linker, VariantType::VariantArray, "var.is_array");
    variant_typecheck!(linker, VariantType::Dictionary, "var.is_dictionary");
    variant_typecheck!(linker, VariantType::GodotString, "var.is_string");
    variant_typecheck!(linker, VariantType::Object, "var.is_object");

    variant_convert!(linker, i32, ("var.from_i32", "var.to_i32"));
    variant_convert!(linker, i64, ("var.from_i64", "var.to_i64"));
    variant_convert!(linker, f32, ("var.from_f32", "var.to_f32"));
    variant_convert!(linker, f64, ("var.from_f64", "var.to_f64"));
    linker.func_wrap(GODOT_MODULE, "var.from_bool", |v: i32| {
        variant_to_externref((v != 0).to_variant())
    })?;
    linker.func_wrap(GODOT_MODULE, "var.to_bool", |v| {
        externref_to_object::<bool>(v).map(|v| if v { 1 } else { 0 })
    })?;
    variant_convert!(linker, Vector2 => (x: f32, y: f32), ("var.from_vec2", "var.to_vec2"));
    variant_convert!(linker, Vector3 => (x: f32, y: f32, z: f32), ("var.from_vec3", "var.to_vec3"));

    object_new!(linker, VariantArray<Unique>, "arr.create");
    object_new!(linker, Dictionary<Unique>, "dict.create");

    object_call!(linker, fn "arr.duplicate"(v: VariantArray) {
        variant_to_externref(v.duplicate().owned_to_variant())
    });
    object_call!(linker, fn "dict.duplicate"(v: Dictionary) {
        variant_to_externref(v.duplicate().owned_to_variant())
    });

    object_call!(linker, fn "arr.size"(v: VariantArray) {
        v.len()
    });

    object_call!(linker, fn "arr.get"(v: VariantArray, i: i32) {
        if (i < 0) || (i >= v.len()) {
            return Err(Trap::new("Out of bound"));
        } else {
            variant_to_externref(v.get(i))
        }
    });

    object_call!(linker, fn "arr.set"(v: VariantArray, i: i32, x) {
        let x = externref_to_variant(x)?;
        if (i < 0) || (i >= v.len()) {
            return Err(Trap::new("Out of bound"));
        } else {
            v.set(i, x)
        }
    });

    object_call!(linker, fn "arr.grow"(v: VariantArray, x, n: i32) {
        let x = externref_to_variant(x)?;
        let v = unsafe { v.assume_unique() };
        if n > 0 {
            for _ in 0..n {
                v.push(x.clone());
            }
        } else if n < 0 {
            v.resize(v.len() - n);
        }
        v.len()
    });

    object_call!(linker, fn "arr.fill"(v: VariantArray, i: i32, x, n: i32) {
        if (n < 0) || (i < 0) || ((i + n) > v.len()) {
            return Err(Trap::new("Out of bound"));
        }
        let x = externref_to_variant(x)?;
        for j in i..(i + n) {
            v.set(j, x.clone());
        }
    });

    object_call!(linker, fn "dict.size"(d: Dictionary) {
        d.len()
    });

    object_call!(linker, fn "dict.key_in"(d: Dictionary, k) {
        d.contains(externref_to_variant(k)?) as i32
    });

    object_call!(linker, fn "dict.get"(d: Dictionary, k) {
        variant_to_externref(d.get(externref_to_variant(k)?))
    });

    object_call!(linker, fn "dict.set"(d: Dictionary, k, v) {
        d.update(externref_to_variant(k)?, externref_to_variant(v)?);
    });

    object_call!(linker, fn "dict.delete"(d: Dictionary, k) {
        unsafe { d.assume_unique() }.erase(externref_to_variant(k)?);
    });

    object_call!(linker, fn "dict.clear"(d: Dictionary) {
        unsafe { d.assume_unique() }.clear();
    });

    object_call!(linker, fn "dict.iter"(mut ctx, d: Dictionary, f: Option<Func>) {
        let f = f.ok_or_else(|| Trap::new("Function is null"))
            .and_then(|f| {
                f.typed::<(Option<ExternRef>, Option<ExternRef>), i32, _>(&ctx)
                    .map_err(Trap::from)
            })?;
        for (k, v) in d.iter() {
            if f.call(&mut ctx, (variant_to_externref(k), variant_to_externref(v)))?
                != 0
            {
                break;
            }
        }
    });

    linker.func_wrap(
        GODOT_MODULE,
        "str.create",
        |mut ctx: Caller<_>, s: u32, n: u32| {
            let mem = match ctx.get_export("memory").and_then(|mem| mem.into_memory()) {
                Some(mem) => mem,
                None => return Err(Trap::new("No memory exported")),
            }
            .data(&ctx);

            if let Some(s) = mem.get((s as usize)..((s + n) as usize)) {
                Ok(variant_to_externref(
                    GodotString::from_str(String::from_utf8_lossy(s)).to_variant(),
                ))
            } else {
                Err(Trap::new("Out of bound"))
            }
        },
    )?;

    object_call!(linker, fn "str.read"(mut ctx, v: GodotString, s: u32, n: u32) {
        let mem = match ctx.get_export("memory").and_then(|mem| mem.into_memory()) {
            Some(mem) => mem,
            None => return Err(Trap::new("No memory exported")),
        }
        .data_mut(&mut ctx);

        if let Some(s) = mem.get_mut((s as usize)..((s + n) as usize)) {
            write!(&mut *s, "{}", v).map_err(|e| Trap::from(anyhow::Error::new(e)))
        } else {
            return Err(Trap::new("Out of bound"));
        }
    });

    object_call!(linker, fn "str.size"(s: GodotString) {
        s.len() as u32
    });

    object_call!(linker, fn "str.is_valid_float"(s: GodotString) {
        s.is_valid_float() as i32
    });

    object_call!(linker, fn "str.is_valid_integer"(s: GodotString) {
        s.is_valid_integer() as i32
    });

    object_call!(linker, fn "str.is_valid_hex_number"(s: GodotString, p: i32) {
        s.is_valid_hex_number(p != 0) as i32
    });

    object_call!(linker, fn "str.to_i32"(s: GodotString) {
        s.to_i32()
    });

    object_call!(linker, fn "str.to_f32"(s: GodotString) {
        s.to_f32()
    });

    object_call!(linker, fn "str.to_f64"(s: GodotString) {
        s.to_f64()
    });

    object_call!(linker, fn "str.hex_to_int"(s: GodotString) {
        s.hex_to_int()
    });

    object_call!(linker, fn "str.to_lower"(s: GodotString) {
        variant_to_externref(s.to_lowercase().to_variant())
    });

    object_call!(linker, fn "str.to_upper"(s: GodotString) {
        variant_to_externref(s.to_uppercase().to_variant())
    });

    object_call!(linker, fn "str.capitalize"(s: GodotString) {
        variant_to_externref(s.capitalize().to_variant())
    });

    object_call!(linker, fn "str.c_escape"(s: GodotString) {
        variant_to_externref(s.c_escape().to_variant())
    });

    object_call!(linker, fn "str.c_unescape"(s: GodotString) {
        variant_to_externref(s.c_unescape().to_variant())
    });

    object_call!(linker, fn "str.http_escape"(s: GodotString) {
        variant_to_externref(s.http_escape().to_variant())
    });

    object_call!(linker, fn "str.http_unescape"(s: GodotString) {
        variant_to_externref(s.http_unescape().to_variant())
    });

    object_call!(linker, fn "str.xml_escape"(s: GodotString) {
        variant_to_externref(s.xml_escape().to_variant())
    });

    object_call!(linker, fn "str.xml_escape_with_quotes"(s: GodotString) {
        variant_to_externref(s.xml_escape_with_quotes().to_variant())
    });

    object_call!(linker, fn "str.xml_unescape"(s: GodotString) {
        variant_to_externref(s.xml_unescape().to_variant())
    });

    object_call!(linker, fn "str.percent_encode"(s: GodotString) {
        variant_to_externref(s.percent_encode().to_variant())
    });

    object_call!(linker, fn "str.percent_decode"(s: GodotString) {
        variant_to_externref(s.percent_decode().to_variant())
    });

    object_call!(linker, fn "str.begins_with"(s: GodotString, o) {
        s.begins_with(&externref_to_object(o)?) as i32
    });

    object_call!(linker, fn "str.ends_with"(s: GodotString, o) {
        s.ends_with(&externref_to_object(o)?) as i32
    });

    Ok(())
}
