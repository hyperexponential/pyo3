//! An experiment module which has all codes related only to #[pyclass]
use crate::class::methods::{PyMethodDefType, PyMethodsProtocol};
use crate::conversion::{AsPyPointer, FromPyPointer, ToPyObject};
use crate::exceptions::RuntimeError;
use crate::pyclass_slots::{PyClassDict, PyClassWeakRef};
use crate::type_object::{type_flags, PyObjectLayout, PyObjectSizedLayout, PyTypeObject};
use crate::types::PyAny;
use crate::{class, ffi, gil, PyErr, PyObject, PyResult, PyTypeInfo, Python};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::os::raw::c_void;
use std::ptr::{self, NonNull};

#[inline]
pub(crate) unsafe fn default_alloc<T: PyTypeInfo>() -> *mut ffi::PyObject {
    let tp_ptr = T::type_object();
    if T::FLAGS & type_flags::EXTENDED != 0
        && <T::BaseType as PyTypeInfo>::ConcreteLayout::IS_NATIVE_TYPE
    {
        let base_tp_ptr = <T::BaseType as PyTypeInfo>::type_object();
        if let Some(base_new) = (*base_tp_ptr).tp_new {
            return base_new(tp_ptr, ptr::null_mut(), ptr::null_mut());
        }
    }
    let alloc = (*tp_ptr).tp_alloc.unwrap_or(ffi::PyType_GenericAlloc);
    alloc(tp_ptr, 0)
}

/// A trait that enables custome alloc/dealloc implementations for pyclasses.
pub trait PyClassAlloc: PyTypeInfo + Sized {
    unsafe fn alloc(_py: Python) -> *mut Self::ConcreteLayout {
        default_alloc::<Self>() as _
    }

    unsafe fn dealloc(py: Python, self_: *mut Self::ConcreteLayout) {
        (*self_).py_drop(py);
        let obj = self_ as _;
        if ffi::PyObject_CallFinalizerFromDealloc(obj) < 0 {
            return;
        }

        match Self::type_object().tp_free {
            Some(free) => free(obj as *mut c_void),
            None => tp_free_fallback(obj),
        }
    }
}

#[doc(hidden)]
pub unsafe fn tp_free_fallback(obj: *mut ffi::PyObject) {
    let ty = ffi::Py_TYPE(obj);
    if ffi::PyType_IS_GC(ty) != 0 {
        ffi::PyObject_GC_Del(obj as *mut c_void);
    } else {
        ffi::PyObject_Free(obj as *mut c_void);
    }

    // For heap types, PyType_GenericAlloc calls INCREF on the type objects,
    // so we need to call DECREF here:
    if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_HEAPTYPE) != 0 {
        ffi::Py_DECREF(ty as *mut ffi::PyObject);
    }
}

/// If `PyClass` is implemented for `T`, then we can use `T` in the Python world,
/// via `PyClassShell`.
///
/// `#[pyclass]` attribute automatically implement this trait for your Rust struct,
/// so you don't have to use this trait directly.
pub trait PyClass:
    PyTypeInfo<ConcreteLayout = PyClassShell<Self>> + Sized + PyClassAlloc + PyMethodsProtocol
{
    type Dict: PyClassDict;
    type WeakRef: PyClassWeakRef;
}

unsafe impl<T> PyTypeObject for T
where
    T: PyClass,
{
    fn init_type() -> NonNull<ffi::PyTypeObject> {
        let type_object = unsafe { <Self as PyTypeInfo>::type_object() };

        if (type_object.tp_flags & ffi::Py_TPFLAGS_READY) == 0 {
            // automatically initialize the class on-demand
            let gil = Python::acquire_gil();
            let py = gil.python();

            initialize_type::<Self>(py, <Self as PyTypeInfo>::MODULE).unwrap_or_else(|e| {
                e.print(py);
                panic!("An error occurred while initializing class {}", Self::NAME)
            });
        }

        unsafe { NonNull::new_unchecked(type_object) }
    }
}

/// `PyClassShell` represents the concrete layout of `PyClass` in the Python heap.
///
/// You can use it for testing your `#[pyclass]` correctly works.
///
/// ```
/// # use pyo3::prelude::*;
/// # use pyo3::{py_run, PyClassShell};
/// #[pyclass]
/// struct Book {
///     #[pyo3(get)]
///     name: &'static str,
///     author: &'static str,
/// }
/// let gil = Python::acquire_gil();
/// let py = gil.python();
/// let book = Book {
///     name: "The Man in the High Castle",
///     author: "Philip Kindred Dick",
/// };
/// let book_shell = PyClassShell::new_ref(py, book).unwrap();
/// py_run!(py, book_shell, "assert book_shell.name[-6:] == 'Castle'");
/// ```
#[repr(C)]
pub struct PyClassShell<T: PyClass> {
    ob_base: <T::BaseType as PyTypeInfo>::ConcreteLayout,
    pyclass: ManuallyDrop<T>,
    dict: T::Dict,
    weakref: T::WeakRef,
}

impl<T: PyClass> PyClassShell<T> {
    /// Make new `PyClassShell` on the Python heap and returns the reference of it.
    pub fn new_ref(py: Python, value: impl IntoInitializer<T>) -> PyResult<&Self>
    where
        <T::BaseType as PyTypeInfo>::ConcreteLayout:
            crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        unsafe {
            let initializer = value.into_initializer()?;
            let self_ = initializer.create_shell(py)?;
            FromPyPointer::from_owned_ptr_or_err(py, self_ as _)
        }
    }

    /// Make new `PyClassShell` on the Python heap and returns the mutable reference of it.
    pub fn new_mut(py: Python, value: impl IntoInitializer<T>) -> PyResult<&mut Self>
    where
        <T::BaseType as PyTypeInfo>::ConcreteLayout:
            crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        unsafe {
            let initializer = value.into_initializer()?;
            let self_ = initializer.create_shell(py)?;
            FromPyPointer::from_owned_ptr_or_err(py, self_ as _)
        }
    }

    /// Get the reference of base object.
    pub fn get_super(&self) -> &<T::BaseType as PyTypeInfo>::ConcreteLayout {
        &self.ob_base
    }

    /// Get the mutable reference of base object.
    pub fn get_super_mut(&mut self) -> &mut <T::BaseType as PyTypeInfo>::ConcreteLayout {
        &mut self.ob_base
    }

    unsafe fn new(py: Python) -> PyResult<*mut Self>
    where
        <T::BaseType as PyTypeInfo>::ConcreteLayout:
            crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        <T::BaseType as PyTypeObject>::init_type();
        T::init_type();
        let base = T::alloc(py);
        if base.is_null() {
            return Err(PyErr::fetch(py));
        }
        let self_ = base as *mut Self;
        (*self_).dict = T::Dict::new();
        (*self_).weakref = T::WeakRef::new();
        Ok(self_)
    }
}

impl<T: PyClass> PyObjectLayout<T> for PyClassShell<T> {
    const NEED_INIT: bool = std::mem::size_of::<T>() != 0;
    const IS_NATIVE_TYPE: bool = false;
    fn get_super_or(&mut self) -> Option<&mut <T::BaseType as PyTypeInfo>::ConcreteLayout> {
        Some(&mut self.ob_base)
    }
    unsafe fn internal_ref_cast(obj: &PyAny) -> &T {
        let shell = obj.as_ptr() as *const PyClassShell<T>;
        &(*shell).pyclass
    }
    unsafe fn internal_mut_cast(obj: &PyAny) -> &mut T {
        let shell = obj.as_ptr() as *const PyClassShell<T> as *mut PyClassShell<T>;
        &mut (*shell).pyclass
    }
    unsafe fn py_drop(&mut self, py: Python) {
        ManuallyDrop::drop(&mut self.pyclass);
        self.dict.clear_dict(py);
        self.weakref.clear_weakrefs(self.as_ptr(), py);
        self.ob_base.py_drop(py);
    }
    unsafe fn py_init(&mut self, value: T) {
        self.pyclass = ManuallyDrop::new(value);
    }
}

impl<T: PyClass> PyObjectSizedLayout<T> for PyClassShell<T> {}

impl<T: PyClass> AsPyPointer for PyClassShell<T> {
    fn as_ptr(&self) -> *mut ffi::PyObject {
        (self as *const _) as *mut _
    }
}

impl<T: PyClass> std::ops::Deref for PyClassShell<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.pyclass.deref()
    }
}

impl<T: PyClass> std::ops::DerefMut for PyClassShell<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.pyclass.deref_mut()
    }
}

impl<T: PyClass> ToPyObject for &PyClassShell<T> {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        unsafe { PyObject::from_borrowed_ptr(py, self.as_ptr()) }
    }
}

impl<T: PyClass> ToPyObject for &mut PyClassShell<T> {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        unsafe { PyObject::from_borrowed_ptr(py, self.as_ptr()) }
    }
}

unsafe impl<'p, T> FromPyPointer<'p> for &'p PyClassShell<T>
where
    T: PyClass,
{
    unsafe fn from_owned_ptr_or_opt(py: Python<'p>, ptr: *mut ffi::PyObject) -> Option<Self> {
        NonNull::new(ptr).map(|p| &*(gil::register_owned(py, p).as_ptr() as *const PyClassShell<T>))
    }
    unsafe fn from_borrowed_ptr_or_opt(py: Python<'p>, ptr: *mut ffi::PyObject) -> Option<Self> {
        NonNull::new(ptr)
            .map(|p| &*(gil::register_borrowed(py, p).as_ptr() as *const PyClassShell<T>))
    }
}

unsafe impl<'p, T> FromPyPointer<'p> for &'p mut PyClassShell<T>
where
    T: PyClass,
{
    unsafe fn from_owned_ptr_or_opt(py: Python<'p>, ptr: *mut ffi::PyObject) -> Option<Self> {
        NonNull::new(ptr).map(|p| {
            &mut *(gil::register_owned(py, p).as_ptr() as *const PyClassShell<T> as *mut _)
        })
    }
    unsafe fn from_borrowed_ptr_or_opt(py: Python<'p>, ptr: *mut ffi::PyObject) -> Option<Self> {
        NonNull::new(ptr).map(|p| {
            &mut *(gil::register_borrowed(py, p).as_ptr() as *const PyClassShell<T> as *mut _)
        })
    }
}

/// A speciall initializer for `PyClassShell<T>`, which enables `super().__init__`
/// in Rust code.
///
/// You have to use it only when your `#[pyclass]` extends another `#[pyclass]`.
///
/// ```
/// # use pyo3::prelude::*;
/// # use pyo3::py_run;
/// #[pyclass]
/// struct BaseClass {
///     #[pyo3(get)]
///     basename: &'static str,
/// }
/// #[pyclass(extends=BaseClass)]
/// struct SubClass {
///     #[pyo3(get)]
///     subname: &'static str,
/// }
/// #[pymethods]
/// impl SubClass {
///     #[new]
///     fn new() -> PyClassInitializer<Self> {
///         let mut init = PyClassInitializer::from_value(SubClass{ subname: "sub"  });
///         init.get_super().init(BaseClass { basename: "base" });
///         init
///     }
/// }
/// let gil = Python::acquire_gil();
/// let py = gil.python();
/// let _basetype = py.get_type::<BaseClass>();
/// let typeobj = py.get_type::<SubClass>();
/// let inst = typeobj.call((), None).unwrap();
/// py_run!(py, inst, "assert inst.basename == 'base'; assert inst.subname == 'sub'");
/// ```
pub struct PyClassInitializer<T: PyTypeInfo> {
    init: Option<T>,
    super_init: Option<*mut PyClassInitializer<T::BaseType>>,
}

impl<T: PyTypeInfo> PyClassInitializer<T> {
    /// Construct `PyClassInitializer` for specified value `value`.
    ///
    /// Same as
    /// ```ignore
    /// let mut init = PyClassInitializer::<T>();
    /// init.init(value);
    /// ```
    pub fn from_value(value: T) -> Self {
        PyClassInitializer {
            init: Some(value),
            super_init: None,
        }
    }

    /// Make new `PyClassInitializer` with empty values.
    pub fn new() -> Self {
        PyClassInitializer {
            init: None,
            super_init: None,
        }
    }

    #[must_use]
    #[doc(hiddden)]
    pub fn init_class(self, shell: &mut T::ConcreteLayout) -> PyResult<()> {
        macro_rules! raise_err {
            ($name: path) => {
                return Err(PyErr::new::<RuntimeError, _>(format!(
                    "Base class '{}' is not initialized",
                    $name
                )));
            };
        }
        let PyClassInitializer { init, super_init } = self;
        if let Some(value) = init {
            unsafe { shell.py_init(value) };
        } else if T::ConcreteLayout::NEED_INIT {
            raise_err!(T::NAME);
        }
        if let Some(super_init) = super_init {
            let super_init = unsafe { Box::from_raw(super_init) };
            if let Some(super_obj) = shell.get_super_or() {
                super_init.init_class(super_obj)?;
            }
        } else if <T::BaseType as PyTypeInfo>::ConcreteLayout::NEED_INIT {
            raise_err!(T::BaseType::NAME)
        }
        Ok(())
    }

    /// Pass the value that you use in Python to the initializer.
    pub fn init(&mut self, value: T) {
        self.init = Some(value);
    }

    /// Get the initializer for the base object.
    /// Resembles `super().__init__()` in Python.
    pub fn get_super(&mut self) -> &mut PyClassInitializer<T::BaseType> {
        if let Some(super_init) = self.super_init {
            return unsafe { &mut *super_init };
        }
        let super_init = Box::into_raw(Box::new(PyClassInitializer::new()));
        self.super_init = Some(super_init);
        unsafe { &mut *super_init }
    }

    #[doc(hidden)]
    pub unsafe fn create_shell(self, py: Python) -> PyResult<*mut PyClassShell<T>>
    where
        T: PyClass,
        <T::BaseType as PyTypeInfo>::ConcreteLayout:
            crate::type_object::PyObjectSizedLayout<T::BaseType>,
    {
        let shell = PyClassShell::new(py)?;
        self.init_class(&mut *shell)?;
        Ok(shell)
    }
}

/// Represets that we can convert the type to `PyClassInitializer`.
///
/// It is mainly used in our proc-macro code.
pub trait IntoInitializer<T: PyClass> {
    fn into_initializer(self) -> PyResult<PyClassInitializer<T>>;
}

impl<T: PyClass> IntoInitializer<T> for T {
    fn into_initializer(self) -> PyResult<PyClassInitializer<T>> {
        Ok(PyClassInitializer::from_value(self))
    }
}

impl<T: PyClass> IntoInitializer<T> for PyResult<T> {
    fn into_initializer(self) -> PyResult<PyClassInitializer<T>> {
        self.map(PyClassInitializer::from_value)
    }
}

impl<T: PyClass> IntoInitializer<T> for PyClassInitializer<T> {
    fn into_initializer(self) -> PyResult<PyClassInitializer<T>> {
        Ok(self)
    }
}

impl<T: PyClass> IntoInitializer<T> for PyResult<PyClassInitializer<T>> {
    fn into_initializer(self) -> PyResult<PyClassInitializer<T>> {
        self
    }
}

/// Register new type in python object system.
#[cfg(not(Py_LIMITED_API))]
pub fn initialize_type<T>(py: Python, module_name: Option<&str>) -> PyResult<*mut ffi::PyTypeObject>
where
    T: PyClass,
{
    let type_object: &mut ffi::PyTypeObject = unsafe { T::type_object() };
    let base_type_object: &mut ffi::PyTypeObject =
        unsafe { <T::BaseType as PyTypeInfo>::type_object() };

    // PyPy will segfault if passed only a nul terminator as `tp_doc`.
    // ptr::null() is OK though.
    if T::DESCRIPTION == "\0" {
        type_object.tp_doc = ptr::null();
    } else {
        type_object.tp_doc = T::DESCRIPTION.as_ptr() as *const _;
    };

    type_object.tp_base = base_type_object;

    let name = match module_name {
        Some(module_name) => format!("{}.{}", module_name, T::NAME),
        None => T::NAME.to_string(),
    };
    let name = CString::new(name).expect("Module name/type name must not contain NUL byte");
    type_object.tp_name = name.into_raw();

    // dealloc
    unsafe extern "C" fn tp_dealloc_callback<T>(obj: *mut ffi::PyObject)
    where
        T: PyClassAlloc,
    {
        let py = Python::assume_gil_acquired();
        let _pool = gil::GILPool::new_no_pointers(py);
        <T as PyClassAlloc>::dealloc(py, (obj as *mut T::ConcreteLayout) as _)
    }
    type_object.tp_dealloc = Some(tp_dealloc_callback::<T>);

    // type size
    type_object.tp_basicsize = std::mem::size_of::<T::ConcreteLayout>() as ffi::Py_ssize_t;

    let mut offset = type_object.tp_basicsize;

    // __dict__ support
    if let Some(dict_offset) = T::Dict::OFFSET {
        offset += dict_offset as ffi::Py_ssize_t;
        type_object.tp_dictoffset = offset;
    }

    // weakref support
    if let Some(weakref_offset) = T::WeakRef::OFFSET {
        offset += weakref_offset as ffi::Py_ssize_t;
        type_object.tp_weaklistoffset = offset;
    }

    // GC support
    <T as class::gc::PyGCProtocolImpl>::update_type_object(type_object);

    // descriptor protocol
    <T as class::descr::PyDescrProtocolImpl>::tp_as_descr(type_object);

    // iterator methods
    <T as class::iter::PyIterProtocolImpl>::tp_as_iter(type_object);

    // basic methods
    <T as class::basic::PyObjectProtocolImpl>::tp_as_object(type_object);

    fn to_ptr<T>(value: Option<T>) -> *mut T {
        value
            .map(|v| Box::into_raw(Box::new(v)))
            .unwrap_or_else(ptr::null_mut)
    }

    // number methods
    type_object.tp_as_number = to_ptr(<T as class::number::PyNumberProtocolImpl>::tp_as_number());
    // mapping methods
    type_object.tp_as_mapping =
        to_ptr(<T as class::mapping::PyMappingProtocolImpl>::tp_as_mapping());
    // sequence methods
    type_object.tp_as_sequence =
        to_ptr(<T as class::sequence::PySequenceProtocolImpl>::tp_as_sequence());
    // async methods
    type_object.tp_as_async = to_ptr(<T as class::pyasync::PyAsyncProtocolImpl>::tp_as_async());
    // buffer protocol
    type_object.tp_as_buffer = to_ptr(<T as class::buffer::PyBufferProtocolImpl>::tp_as_buffer());

    // normal methods
    let (new, call, mut methods) = py_class_method_defs::<T>();
    if !methods.is_empty() {
        methods.push(ffi::PyMethodDef_INIT);
        type_object.tp_methods = Box::into_raw(methods.into_boxed_slice()) as *mut _;
    }

    // __new__ method
    type_object.tp_new = new;
    // __call__ method
    type_object.tp_call = call;

    // properties
    let mut props = py_class_properties::<T>();

    if T::Dict::OFFSET.is_some() {
        props.push(ffi::PyGetSetDef_DICT);
    }
    if !props.is_empty() {
        props.push(ffi::PyGetSetDef_INIT);
        type_object.tp_getset = Box::into_raw(props.into_boxed_slice()) as *mut _;
    }

    // set type flags
    py_class_flags::<T>(type_object);

    // register type object
    unsafe {
        if ffi::PyType_Ready(type_object) == 0 {
            Ok(type_object as *mut ffi::PyTypeObject)
        } else {
            PyErr::fetch(py).into()
        }
    }
}

fn py_class_flags<T: PyTypeInfo>(type_object: &mut ffi::PyTypeObject) {
    if type_object.tp_traverse != None
        || type_object.tp_clear != None
        || T::FLAGS & type_flags::GC != 0
    {
        type_object.tp_flags = ffi::Py_TPFLAGS_DEFAULT | ffi::Py_TPFLAGS_HAVE_GC;
    } else {
        type_object.tp_flags = ffi::Py_TPFLAGS_DEFAULT;
    }
    if T::FLAGS & type_flags::BASETYPE != 0 {
        type_object.tp_flags |= ffi::Py_TPFLAGS_BASETYPE;
    }
}

fn py_class_method_defs<T: PyMethodsProtocol>() -> (
    Option<ffi::newfunc>,
    Option<ffi::PyCFunctionWithKeywords>,
    Vec<ffi::PyMethodDef>,
) {
    let mut defs = Vec::new();
    let mut call = None;
    let mut new = None;

    for def in T::py_methods() {
        match *def {
            PyMethodDefType::New(ref def) => {
                if let class::methods::PyMethodType::PyNewFunc(meth) = def.ml_meth {
                    new = Some(meth)
                }
            }
            PyMethodDefType::Call(ref def) => {
                if let class::methods::PyMethodType::PyCFunctionWithKeywords(meth) = def.ml_meth {
                    call = Some(meth)
                } else {
                    panic!("Method type is not supoorted by tp_call slot")
                }
            }
            PyMethodDefType::Method(ref def)
            | PyMethodDefType::Class(ref def)
            | PyMethodDefType::Static(ref def) => {
                defs.push(def.as_method_def());
            }
            _ => (),
        }
    }

    for def in <T as class::basic::PyObjectProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }
    for def in <T as class::context::PyContextProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }
    for def in <T as class::mapping::PyMappingProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }
    for def in <T as class::number::PyNumberProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }
    for def in <T as class::descr::PyDescrProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }

    py_class_async_methods::<T>(&mut defs);

    (new, call, defs)
}

fn py_class_async_methods<T>(defs: &mut Vec<ffi::PyMethodDef>) {
    for def in <T as class::pyasync::PyAsyncProtocolImpl>::methods() {
        defs.push(def.as_method_def());
    }
}

fn py_class_properties<T: PyMethodsProtocol>() -> Vec<ffi::PyGetSetDef> {
    let mut defs = std::collections::HashMap::new();

    for def in T::py_methods() {
        match *def {
            PyMethodDefType::Getter(ref getter) => {
                let name = getter.name.to_string();
                if !defs.contains_key(&name) {
                    let _ = defs.insert(name.clone(), ffi::PyGetSetDef_INIT);
                }
                let def = defs.get_mut(&name).expect("Failed to call get_mut");
                getter.copy_to(def);
            }
            PyMethodDefType::Setter(ref setter) => {
                let name = setter.name.to_string();
                if !defs.contains_key(&name) {
                    let _ = defs.insert(name.clone(), ffi::PyGetSetDef_INIT);
                }
                let def = defs.get_mut(&name).expect("Failed to call get_mut");
                setter.copy_to(def);
            }
            _ => (),
        }
    }

    defs.values().cloned().collect()
}
