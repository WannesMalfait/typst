//! Computational utility functions.

mod color;
mod data;
mod math;
mod string;

pub use color::*;
pub use data::*;
pub use math::*;
pub use string::*;

use crate::eval::{Eval, Scopes, Vm};
use crate::library::prelude::*;
use crate::source::Source;

/// The name of a value's type.
pub fn type_(_: &mut Vm, args: &mut Args) -> TypResult<Value> {
    Ok(args.expect::<Value>("value")?.type_name().into())
}

/// Ensure that a condition is fulfilled.
pub fn assert(_: &mut Vm, args: &mut Args) -> TypResult<Value> {
    let Spanned { v, span } = args.expect::<Spanned<bool>>("condition")?;
    if !v {
        bail!(span, "assertion failed");
    }
    Ok(Value::None)
}

/// Evaluate a string as Typst markup.
pub fn eval(vm: &mut Vm, args: &mut Args) -> TypResult<Value> {
    let Spanned { v: text, span } = args.expect::<Spanned<String>>("source")?;

    // Parse the source and set a synthetic span for all nodes.
    let source = Source::synthesized(text, span);
    let ast = source.ast()?;

    // Evaluate the source.
    let std = &vm.world.config().std;
    let scopes = Scopes::new(Some(std));
    let mut sub = Vm::new(vm.world, vec![], scopes);
    let result = ast.eval(&mut sub);

    // Handle control flow.
    if let Some(flow) = sub.flow {
        return Err(flow.forbidden());
    }

    Ok(Value::Content(result?))
}
