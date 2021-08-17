// Copyright (c) 2017-present PyO3 Project and Contributors
//
// based on Daniel Grunwald's https://github.com/dgrunwald/rust-cpython

//! Functionality for the code generated by the derive backend

use crate::err::{PyErr, PyResult};
use crate::exceptions::PyTypeError;
use crate::pyclass::PyClass;
use crate::types::{PyAny, PyDict, PyModule, PyString, PyTuple};
use crate::{ffi, PyCell, Python};
use std::cell::UnsafeCell;

#[derive(Debug)]
pub struct KeywordOnlyParameterDescription {
    pub name: &'static str,
    pub required: bool,
}

/// Function argument specification for a `#[pyfunction]` or `#[pymethod]`.
#[derive(Debug)]
pub struct FunctionDescription {
    pub cls_name: Option<&'static str>,
    pub func_name: &'static str,
    pub positional_parameter_names: &'static [&'static str],
    pub positional_only_parameters: usize,
    pub required_positional_parameters: usize,
    pub keyword_only_parameters: &'static [KeywordOnlyParameterDescription],
    pub accept_varargs: bool,
    pub accept_varkeywords: bool,
}

impl FunctionDescription {
    fn full_name(&self) -> String {
        if let Some(cls_name) = self.cls_name {
            format!("{}.{}()", cls_name, self.func_name)
        } else {
            format!("{}()", self.func_name)
        }
    }

    /// Extracts the `args` and `kwargs` provided into `output`, according to this function
    /// definition.
    ///
    /// `output` must have the same length as this function has positional and keyword-only
    /// parameters (as per the `positional_parameter_names` and `keyword_only_parameters`
    /// respectively).
    ///
    /// If `accept_varargs` or `accept_varkeywords`, then the returned `&PyTuple` and `&PyDict` may
    /// be `Some` if there are extra arguments.
    ///
    /// Unexpected, duplicate or invalid arguments will cause this function to return `TypeError`.
    pub fn extract_arguments<'p>(
        &self,
        py: Python<'p>,
        mut args: impl ExactSizeIterator<Item = &'p PyAny>,
        kwargs: Option<impl Iterator<Item = (&'p PyAny, &'p PyAny)>>,
        output: &mut [Option<&'p PyAny>],
    ) -> PyResult<(Option<&'p PyTuple>, Option<&'p PyDict>)> {
        let num_positional_parameters = self.positional_parameter_names.len();

        debug_assert!(self.positional_only_parameters <= num_positional_parameters);
        debug_assert!(self.required_positional_parameters <= num_positional_parameters);
        debug_assert_eq!(
            output.len(),
            num_positional_parameters + self.keyword_only_parameters.len()
        );

        // Handle positional arguments
        let args_provided = {
            let args_provided = args.len();
            if self.accept_varargs {
                std::cmp::min(num_positional_parameters, args_provided)
            } else if args_provided > num_positional_parameters {
                return Err(self.too_many_positional_arguments(args_provided));
            } else {
                args_provided
            }
        };

        // Copy positional arguments into output
        for (out, arg) in output[..args_provided].iter_mut().zip(args.by_ref()) {
            *out = Some(arg);
        }

        // Collect varargs into tuple
        let varargs = if self.accept_varargs {
            Some(PyTuple::new(py, args))
        } else {
            None
        };

        // Handle keyword arguments
        let varkeywords = match (kwargs, self.accept_varkeywords) {
            (Some(kwargs), true) => {
                let mut varkeywords = None;
                self.extract_keyword_arguments(kwargs, output, |name, value| {
                    varkeywords
                        .get_or_insert_with(|| PyDict::new(py))
                        .set_item(name, value)
                })?;
                varkeywords
            }
            (Some(kwargs), false) => {
                self.extract_keyword_arguments(kwargs, output, |name, _| {
                    Err(self.unexpected_keyword_argument(name))
                })?;
                None
            }
            (None, _) => None,
        };

        // Check that there's sufficient positional arguments once keyword arguments are specified
        if args_provided < self.required_positional_parameters {
            let missing_positional_arguments: Vec<_> = self
                .positional_parameter_names
                .iter()
                .take(self.required_positional_parameters)
                .zip(output.iter())
                .filter_map(|(param, out)| if out.is_none() { Some(*param) } else { None })
                .collect();
            if !missing_positional_arguments.is_empty() {
                return Err(
                    self.missing_required_arguments("positional", &missing_positional_arguments)
                );
            }
        }

        // Check no missing required keyword arguments
        let missing_keyword_only_arguments: Vec<_> = self
            .keyword_only_parameters
            .iter()
            .zip(&output[num_positional_parameters..])
            .filter_map(|(keyword_desc, out)| {
                if keyword_desc.required && out.is_none() {
                    Some(keyword_desc.name)
                } else {
                    None
                }
            })
            .collect();

        if !missing_keyword_only_arguments.is_empty() {
            return Err(self.missing_required_arguments("keyword", &missing_keyword_only_arguments));
        }

        Ok((varargs, varkeywords))
    }

    #[inline]
    fn extract_keyword_arguments<'p>(
        &self,
        kwargs: impl Iterator<Item = (&'p PyAny, &'p PyAny)>,
        output: &mut [Option<&'p PyAny>],
        mut unexpected_keyword_handler: impl FnMut(&'p PyAny, &'p PyAny) -> PyResult<()>,
    ) -> PyResult<()> {
        let (args_output, kwargs_output) =
            output.split_at_mut(self.positional_parameter_names.len());
        let mut positional_only_keyword_arguments = Vec::new();
        for (kwarg_name, value) in kwargs {
            let utf8_string = match kwarg_name.downcast::<PyString>()?.to_str() {
                Ok(utf8_string) => utf8_string,
                // This keyword is not a UTF8 string: all PyO3 argument names are guaranteed to be
                // UTF8 by construction.
                Err(_) => {
                    unexpected_keyword_handler(kwarg_name, value)?;
                    continue;
                }
            };

            // Compare the keyword name against each parameter in turn. This is exactly the same method
            // which CPython uses to map keyword names. Although it's O(num_parameters), the number of
            // parameters is expected to be small so it's not worth constructing a mapping.
            if let Some(i) = self
                .keyword_only_parameters
                .iter()
                .position(|param| utf8_string == param.name)
            {
                kwargs_output[i] = Some(value);
                continue;
            }

            // Repeat for positional parameters
            if let Some((i, param)) = self
                .positional_parameter_names
                .iter()
                .enumerate()
                .find(|&(_, param)| utf8_string == *param)
            {
                if i < self.positional_only_parameters {
                    positional_only_keyword_arguments.push(*param);
                } else if args_output[i].replace(value).is_some() {
                    return Err(self.multiple_values_for_argument(param));
                }
                continue;
            }

            unexpected_keyword_handler(kwarg_name, value)?;
        }

        if positional_only_keyword_arguments.is_empty() {
            Ok(())
        } else {
            Err(self.positional_only_keyword_arguments(&positional_only_keyword_arguments))
        }
    }

    fn too_many_positional_arguments(&self, args_provided: usize) -> PyErr {
        let was = if args_provided == 1 { "was" } else { "were" };
        let msg = if self.required_positional_parameters != self.positional_parameter_names.len() {
            format!(
                "{} takes from {} to {} positional arguments but {} {} given",
                self.full_name(),
                self.required_positional_parameters,
                self.positional_parameter_names.len(),
                args_provided,
                was
            )
        } else {
            format!(
                "{} takes {} positional arguments but {} {} given",
                self.full_name(),
                self.positional_parameter_names.len(),
                args_provided,
                was
            )
        };
        PyTypeError::new_err(msg)
    }

    fn multiple_values_for_argument(&self, argument: &str) -> PyErr {
        PyTypeError::new_err(format!(
            "{} got multiple values for argument '{}'",
            self.full_name(),
            argument
        ))
    }

    fn unexpected_keyword_argument(&self, argument: &PyAny) -> PyErr {
        PyTypeError::new_err(format!(
            "{} got an unexpected keyword argument '{}'",
            self.full_name(),
            argument
        ))
    }

    fn positional_only_keyword_arguments(&self, parameter_names: &[&str]) -> PyErr {
        let mut msg = format!(
            "{} got some positional-only arguments passed as keyword arguments: ",
            self.full_name()
        );
        push_parameter_list(&mut msg, parameter_names);
        PyTypeError::new_err(msg)
    }

    fn missing_required_arguments(&self, argument_type: &str, parameter_names: &[&str]) -> PyErr {
        let arguments = if parameter_names.len() == 1 {
            "argument"
        } else {
            "arguments"
        };
        let mut msg = format!(
            "{} missing {} required {} {}: ",
            self.full_name(),
            parameter_names.len(),
            argument_type,
            arguments,
        );
        push_parameter_list(&mut msg, parameter_names);
        PyTypeError::new_err(msg)
    }
}

/// Add the argument name to the error message of an error which occurred during argument extraction
pub fn argument_extraction_error(py: Python, arg_name: &str, error: PyErr) -> PyErr {
    if error.ptype(py) == py.get_type::<PyTypeError>() {
        let reason = error
            .instance(py)
            .str()
            .unwrap_or_else(|_| PyString::new(py, ""));
        PyTypeError::new_err(format!("argument '{}': {}", arg_name, reason))
    } else {
        error
    }
}

/// `Sync` wrapper of `ffi::PyModuleDef`.
pub struct ModuleDef(UnsafeCell<ffi::PyModuleDef>);

unsafe impl Sync for ModuleDef {}

impl ModuleDef {
    /// Make new module defenition with given module name.
    ///
    /// # Safety
    /// `name` and `doc` must be null-terminated strings.
    pub const unsafe fn new(name: &'static str, doc: &'static str) -> Self {
        const INIT: ffi::PyModuleDef = ffi::PyModuleDef {
            m_base: ffi::PyModuleDef_HEAD_INIT,
            m_name: std::ptr::null(),
            m_doc: std::ptr::null(),
            m_size: 0,
            m_methods: std::ptr::null_mut(),
            m_slots: std::ptr::null_mut(),
            m_traverse: None,
            m_clear: None,
            m_free: None,
        };

        ModuleDef(UnsafeCell::new(ffi::PyModuleDef {
            m_name: name.as_ptr() as *const _,
            m_doc: doc.as_ptr() as *const _,
            ..INIT
        }))
    }
    /// Builds a module using user given initializer. Used for `#[pymodule]`.
    pub fn make_module(
        &'static self,
        py: Python,
        initializer: impl Fn(Python, &PyModule) -> PyResult<()>,
    ) -> PyResult<*mut ffi::PyObject> {
        let module =
            unsafe { py.from_owned_ptr_or_err::<PyModule>(ffi::PyModule_Create(self.0.get()))? };
        initializer(py, module)?;
        Ok(crate::IntoPyPointer::into_ptr(module))
    }
}

/// Utility trait to enable &PyClass as a pymethod/function argument
#[doc(hidden)]
pub trait ExtractExt<'a> {
    type Target: crate::FromPyObject<'a>;
}

impl<'a, T> ExtractExt<'a> for T
where
    T: crate::FromPyObject<'a>,
{
    type Target = T;
}

/// A trait for types that can be borrowed from a cell.
///
/// This serves to unify the use of `PyRef` and `PyRefMut` in automatically
/// derived code, since both types can be obtained from a `PyCell`.
#[doc(hidden)]
pub trait TryFromPyCell<'a, T: PyClass>: Sized {
    type Error: Into<PyErr>;
    fn try_from_pycell(cell: &'a crate::PyCell<T>) -> Result<Self, Self::Error>;
}

impl<'a, T, R> TryFromPyCell<'a, T> for R
where
    T: 'a + PyClass,
    R: std::convert::TryFrom<&'a PyCell<T>>,
    R::Error: Into<PyErr>,
{
    type Error = R::Error;
    fn try_from_pycell(cell: &'a crate::PyCell<T>) -> Result<Self, Self::Error> {
        <R as std::convert::TryFrom<&'a PyCell<T>>>::try_from(cell)
    }
}

/// Enum to abstract over the arguments of Python function wrappers.
pub enum PyFunctionArguments<'a> {
    Python(Python<'a>),
    PyModule(&'a PyModule),
}

impl<'a> PyFunctionArguments<'a> {
    pub fn into_py_and_maybe_module(self) -> (Python<'a>, Option<&'a PyModule>) {
        match self {
            PyFunctionArguments::Python(py) => (py, None),
            PyFunctionArguments::PyModule(module) => {
                let py = module.py();
                (py, Some(module))
            }
        }
    }
}

impl<'a> From<Python<'a>> for PyFunctionArguments<'a> {
    fn from(py: Python<'a>) -> PyFunctionArguments<'a> {
        PyFunctionArguments::Python(py)
    }
}

impl<'a> From<&'a PyModule> for PyFunctionArguments<'a> {
    fn from(module: &'a PyModule) -> PyFunctionArguments<'a> {
        PyFunctionArguments::PyModule(module)
    }
}

fn push_parameter_list(msg: &mut String, parameter_names: &[&str]) {
    for (i, parameter) in parameter_names.iter().enumerate() {
        if i != 0 {
            if parameter_names.len() > 2 {
                msg.push(',');
            }

            if i == parameter_names.len() - 1 {
                msg.push_str(" and ")
            } else {
                msg.push(' ')
            }
        }

        msg.push('\'');
        msg.push_str(parameter);
        msg.push('\'');
    }
}

#[cfg(test)]
mod tests {
    use super::push_parameter_list;

    #[test]
    fn push_parameter_list_empty() {
        let mut s = String::new();
        push_parameter_list(&mut s, &[]);
        assert_eq!(&s, "");
    }

    #[test]
    fn push_parameter_list_one() {
        let mut s = String::new();
        push_parameter_list(&mut s, &["a"]);
        assert_eq!(&s, "'a'");
    }

    #[test]
    fn push_parameter_list_two() {
        let mut s = String::new();
        push_parameter_list(&mut s, &["a", "b"]);
        assert_eq!(&s, "'a' and 'b'");
    }

    #[test]
    fn push_parameter_list_three() {
        let mut s = String::new();
        push_parameter_list(&mut s, &["a", "b", "c"]);
        assert_eq!(&s, "'a', 'b', and 'c'");
    }

    #[test]
    fn push_parameter_list_four() {
        let mut s = String::new();
        push_parameter_list(&mut s, &["a", "b", "c", "d"]);
        assert_eq!(&s, "'a', 'b', 'c', and 'd'");
    }
}
