//! Import mechanics

use crate::{
    AsObject, PyObjectRef, PyPayload, PyRef, PyResult, TryFromObject,
    builtins::{PyBaseExceptionRef, PyCode, list, traceback::PyTraceback},
    scope::Scope,
    version::get_git_revision,
    vm::{VirtualMachine, thread},
};

pub(crate) fn init_importlib_base(vm: &mut VirtualMachine) -> PyResult<PyObjectRef> {
    flame_guard!("init importlib");

    // importlib_bootstrap needs these and it inlines checks to sys.modules before calling into
    // import machinery, so this should bring some speedup
    #[cfg(all(feature = "threading", not(target_os = "wasi")))]
    import_builtin(vm, "_thread")?;
    import_builtin(vm, "_warnings")?;
    import_builtin(vm, "_weakref")?;

    let importlib = thread::enter_vm(vm, || {
        let bootstrap = import_frozen(vm, "_frozen_importlib")?;
        let install = bootstrap.get_attr("_install", vm)?;
        let imp = import_builtin(vm, "_imp")?;
        install.call((vm.sys_module.clone(), imp), vm)?;
        Ok(bootstrap)
    })?;
    vm.import_func = importlib.get_attr(identifier!(vm, __import__), vm)?;
    Ok(importlib)
}

pub(crate) fn init_importlib_package(vm: &VirtualMachine, importlib: PyObjectRef) -> PyResult<()> {
    thread::enter_vm(vm, || {
        flame_guard!("install_external");

        // same deal as imports above
        #[cfg(any(not(target_arch = "wasm32"), target_os = "wasi"))]
        import_builtin(vm, crate::stdlib::os::MODULE_NAME)?;
        #[cfg(windows)]
        import_builtin(vm, "winreg")?;
        import_builtin(vm, "_io")?;
        import_builtin(vm, "marshal")?;

        let install_external = importlib.get_attr("_install_external_importers", vm)?;
        install_external.call((), vm)?;
        // Set pyc magic number to commit hash. Should be changed when bytecode will be more stable.
        let importlib_external = vm.import("_frozen_importlib_external", 0)?;
        let mut magic = get_git_revision().into_bytes();
        magic.truncate(4);
        if magic.len() != 4 {
            // os_random is expensive, but this is only ever called once
            magic = rustpython_common::rand::os_random::<4>().to_vec();
        }
        let magic: PyObjectRef = vm.ctx.new_bytes(magic).into();
        importlib_external.set_attr("MAGIC_NUMBER", magic, vm)?;
        let zipimport_res = (|| -> PyResult<()> {
            let zipimport = vm.import("zipimport", 0)?;
            let zipimporter = zipimport.get_attr("zipimporter", vm)?;
            let path_hooks = vm.sys_module.get_attr("path_hooks", vm)?;
            let path_hooks = list::PyListRef::try_from_object(vm, path_hooks)?;
            path_hooks.insert(0, zipimporter);
            Ok(())
        })();
        if zipimport_res.is_err() {
            warn!("couldn't init zipimport")
        }
        Ok(())
    })
}

pub fn make_frozen(vm: &VirtualMachine, name: &str) -> PyResult<PyRef<PyCode>> {
    let frozen = vm.state.frozen.get(name).ok_or_else(|| {
        vm.new_import_error(
            format!("No such frozen object named {name}"),
            vm.ctx.new_str(name),
        )
    })?;
    Ok(vm.ctx.new_code(frozen.code))
}

pub fn import_frozen(vm: &VirtualMachine, module_name: &str) -> PyResult {
    let frozen = make_frozen(vm, module_name)?;
    let module = import_code_obj(vm, module_name, frozen, false)?;
    debug_assert!(module.get_attr(identifier!(vm, __name__), vm).is_ok());
    // TODO: give a correct origname here
    module.set_attr("__origname__", vm.ctx.new_str(module_name.to_owned()), vm)?;
    Ok(module)
}

pub fn import_builtin(vm: &VirtualMachine, module_name: &str) -> PyResult {
    let make_module_func = vm.state.module_inits.get(module_name).ok_or_else(|| {
        vm.new_import_error(
            format!("Cannot import builtin module {module_name}"),
            vm.ctx.new_str(module_name),
        )
    })?;
    let module = make_module_func(vm);
    let sys_modules = vm.sys_module.get_attr("modules", vm)?;
    sys_modules.set_item(module_name, module.as_object().to_owned(), vm)?;
    Ok(module.into())
}

#[cfg(feature = "rustpython-compiler")]
pub fn import_file(
    vm: &VirtualMachine,
    module_name: &str,
    file_path: String,
    content: &str,
) -> PyResult {
    let code = vm
        .compile_with_opts(
            content,
            crate::compiler::Mode::Exec,
            file_path,
            vm.compile_opts(),
        )
        .map_err(|err| vm.new_syntax_error(&err, Some(content)))?;
    import_code_obj(vm, module_name, code, true)
}

#[cfg(feature = "rustpython-compiler")]
pub fn import_source(vm: &VirtualMachine, module_name: &str, content: &str) -> PyResult {
    let code = vm
        .compile_with_opts(
            content,
            crate::compiler::Mode::Exec,
            "<source>".to_owned(),
            vm.compile_opts(),
        )
        .map_err(|err| vm.new_syntax_error(&err, Some(content)))?;
    import_code_obj(vm, module_name, code, false)
}

pub fn import_code_obj(
    vm: &VirtualMachine,
    module_name: &str,
    code_obj: PyRef<PyCode>,
    set_file_attr: bool,
) -> PyResult {
    let attrs = vm.ctx.new_dict();
    attrs.set_item(
        identifier!(vm, __name__),
        vm.ctx.new_str(module_name).into(),
        vm,
    )?;
    if set_file_attr {
        attrs.set_item(
            identifier!(vm, __file__),
            code_obj.source_path.to_object(),
            vm,
        )?;
    }
    let module = vm.new_module(module_name, attrs.clone(), None);

    // Store module in cache to prevent infinite loop with mutual importing libs:
    let sys_modules = vm.sys_module.get_attr("modules", vm)?;
    sys_modules.set_item(module_name, module.clone().into(), vm)?;

    // Execute main code in module:
    let scope = Scope::with_builtins(None, attrs, vm);
    vm.run_code_obj(code_obj, scope)?;
    Ok(module.into())
}

fn remove_importlib_frames_inner(
    vm: &VirtualMachine,
    tb: Option<PyRef<PyTraceback>>,
    always_trim: bool,
) -> (Option<PyRef<PyTraceback>>, bool) {
    let traceback = if let Some(tb) = tb {
        tb
    } else {
        return (None, false);
    };

    let file_name = traceback.frame.code.source_path.as_str();

    let (inner_tb, mut now_in_importlib) =
        remove_importlib_frames_inner(vm, traceback.next.lock().clone(), always_trim);
    if file_name == "_frozen_importlib" || file_name == "_frozen_importlib_external" {
        if traceback.frame.code.obj_name.as_str() == "_call_with_frames_removed" {
            now_in_importlib = true;
        }
        if always_trim || now_in_importlib {
            return (inner_tb, now_in_importlib);
        }
    } else {
        now_in_importlib = false;
    }

    (
        Some(
            PyTraceback::new(
                inner_tb,
                traceback.frame.clone(),
                traceback.lasti,
                traceback.lineno,
            )
            .into_ref(&vm.ctx),
        ),
        now_in_importlib,
    )
}

// TODO: This function should do nothing on verbose mode.
// TODO: Fix this function after making PyTraceback.next mutable
pub fn remove_importlib_frames(vm: &VirtualMachine, exc: &PyBaseExceptionRef) {
    if vm.state.settings.verbose != 0 {
        return;
    }

    let always_trim = exc.fast_isinstance(vm.ctx.exceptions.import_error);

    if let Some(tb) = exc.traceback() {
        let trimmed_tb = remove_importlib_frames_inner(vm, Some(tb), always_trim).0;
        exc.set_traceback_typed(trimmed_tb);
    }
}
