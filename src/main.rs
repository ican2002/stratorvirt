// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};

use machine::{LightMachine, MachineOps, StdMachine};
use machine_manager::{
    cmdline::{check_api_channel, create_args_parser, create_vmconfig},
    config::MachineType,
    config::VmConfig,
    event_loop::EventLoop,
    qmp::QmpChannel,
    signal_handler::{exit_with_code, register_kill_signal, VM_EXIT_GENE_ERR},
    socket::Socket,
    temp_cleaner::TempCleaner,
};
use util::loop_context::EventNotifierHelper;
use util::unix::{parse_uri, UnixPath};
use util::{arg_parser, daemonize::daemonize, logger, set_termi_canon_mode};

error_chain! {
    links {
       Manager(machine_manager::errors::Error, machine_manager::errors::ErrorKind);
       Util(util::errors::Error, util::errors::ErrorKind);
       Machine(machine::errors::Error, machine::errors::ErrorKind);
    }
    foreign_links {
        Io(std::io::Error);
    }
}

quick_main!(run);

fn run() -> Result<()> {
    let cmd_args = create_args_parser().get_matches()?;

    if let Some(logfile_path) = cmd_args.value_of("display log") {
        if logfile_path.is_empty() {
            logger::init_logger_with_env(Some(Box::new(std::io::stdout())))
                .chain_err(|| "Failed to init logger.")?;
        } else {
            let logfile = std::fs::OpenOptions::new()
                .read(false)
                .write(true)
                .append(true)
                .create(true)
                .mode(0o640)
                .open(logfile_path)
                .chain_err(|| "Failed to open log file")?;
            logger::init_logger_with_env(Some(Box::new(logfile)))
                .chain_err(|| "Failed to init logger.")?;
        }
    }

    std::panic::set_hook(Box::new(|panic_msg| {
        set_termi_canon_mode().expect("Failed to set terminal to canonical mode.");

        let panic_file = panic_msg.location().map_or("", |loc| loc.file());
        let panic_line = panic_msg.location().map_or(0, |loc| loc.line());
        if let Some(msg) = panic_msg.payload().downcast_ref::<&str>() {
            error!("Panic at [{}: {}]: {}.", panic_file, panic_line, msg);
        } else {
            error!("Panic at [{}: {}].", panic_file, panic_line);
        }

        // clean temporary file
        TempCleaner::clean();
        exit_with_code(VM_EXIT_GENE_ERR);
    }));

    let mut vm_config: VmConfig = create_vmconfig(&cmd_args)?;
    info!("VmConfig is {:?}", vm_config);

    match real_main(&cmd_args, &mut vm_config) {
        Ok(()) => {
            info!("MainLoop over, Vm exit");
            // clean temporary file
            TempCleaner::clean();
        }
        Err(ref e) => {
            set_termi_canon_mode().expect("Failed to set terminal to canonical mode.");
            if cmd_args.is_present("display log") {
                error!("{}", error_chain::ChainedError::display_chain(e));
            } else {
                write!(
                    &mut std::io::stderr(),
                    "{}",
                    error_chain::ChainedError::display_chain(e)
                )
                .expect("Failed to write to stderr");
            }
            // clean temporary file
            TempCleaner::clean();
            exit_with_code(VM_EXIT_GENE_ERR);
        }
    }

    Ok(())
}

fn real_main(cmd_args: &arg_parser::ArgMatches, vm_config: &mut VmConfig) -> Result<()> {
    TempCleaner::object_init();

    if cmd_args.is_present("daemonize") {
        match daemonize(cmd_args.value_of("pidfile")) {
            Ok(()) => {
                if let Some(pidfile) = cmd_args.value_of("pidfile") {
                    TempCleaner::add_path(pidfile);
                }
                info!("Daemonize mode start!");
            }
            Err(e) => bail!("Daemonize start failed: {}", e),
        }
    } else if cmd_args.value_of("pidfile").is_some() {
        bail!("-pidfile must be used with -daemonize together.");
    }

    QmpChannel::object_init();
    EventLoop::object_init(&vm_config.iothreads)?;
    register_kill_signal();

    let listeners = check_api_channel(&cmd_args, vm_config)?;
    let mut sockets = Vec::new();
    let vm: Arc<Mutex<dyn MachineOps + Send + Sync>> = match vm_config.machine_config.mach_type {
        MachineType::MicroVm => {
            let vm = Arc::new(Mutex::new(
                LightMachine::new(&vm_config).chain_err(|| "Failed to init MicroVM")?,
            ));
            MachineOps::realize(&vm, vm_config, cmd_args.is_present("incoming"))
                .chain_err(|| "Failed to realize micro VM.")?;
            EventLoop::set_manager(vm.clone(), None);

            for listener in listeners {
                sockets.push(Socket::from_unix_listener(listener, Some(vm.clone())));
            }
            vm
        }
        MachineType::StandardVm => {
            let vm = Arc::new(Mutex::new(
                StdMachine::new(&vm_config).chain_err(|| "Failed to init StandardVM")?,
            ));
            MachineOps::realize(&vm, vm_config, cmd_args.is_present("incoming"))
                .chain_err(|| "Failed to realize standard VM.")?;
            EventLoop::set_manager(vm.clone(), None);

            for listener in listeners {
                sockets.push(Socket::from_unix_listener(listener, Some(vm.clone())));
            }
            vm
        }
        MachineType::None => {
            let vm = Arc::new(Mutex::new(
                StdMachine::new(&vm_config).chain_err(|| "Failed to init NoneVM")?,
            ));
            EventLoop::set_manager(vm.clone(), None);
            for listener in listeners {
                sockets.push(Socket::from_unix_listener(listener, Some(vm.clone())));
            }
            vm
        }
    };

    if let Some(uri) = cmd_args.value_of("incoming") {
        if let (UnixPath::File, path) = parse_uri(&uri)? {
            migration::MigrationManager::restore_snapshot(&path)
                .chain_err(|| "Failed to start with incoming migration.")?;
        } else {
            bail!("Unsupported incoming unix path type.")
        }
    }

    for socket in sockets {
        EventLoop::update_event(
            EventNotifierHelper::internal_notifiers(Arc::new(Mutex::new(socket))),
            None,
        )
        .chain_err(|| "Failed to add api event to MainLoop")?;
    }

    vm.lock()
        .unwrap()
        .run(cmd_args.is_present("freeze_cpu"))
        .chain_err(|| "Failed to start VM.")?;

    let balloon_switch_on = vm_config.dev_name.get("balloon").is_some();
    if !cmd_args.is_present("disable-seccomp") {
        vm.lock()
            .unwrap()
            .register_seccomp(balloon_switch_on)
            .chain_err(|| "Failed to register seccomp rules.")?;
    }

    EventLoop::loop_run().chain_err(|| "MainLoop exits unexpectedly: error occurs")?;
    Ok(())
}
