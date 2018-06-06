#![feature(nll)]

//! This example is unix-only

#[cfg(not(unix))]
compile_error!("This example is unix only");

extern crate alpm;
extern crate env_logger;
extern crate log;
extern crate users;

use alpm::{Alpm, Error};
use log::LevelFilter;

use std::fs;
use std::path::Path;
use std::process::Command;

const BASE_PATH: &str = "/tmp/alpm-test";

fn main() -> Result<(), Error> {
    // Make a temporary archlinux installation.
    make_base();

    // Make logging nice
    let mut builder = env_logger::Builder::from_default_env();
    builder
        .filter_level(LevelFilter::Debug)
        .filter_module("tokio_reactor", LevelFilter::Warn)
        .filter_module("tokio_core", LevelFilter::Warn)
        .init();


    let mut alpm = Alpm::new()
        .with_root_path(BASE_PATH)
        .build()?;

    let local_db = alpm.local_database();
    println!("local db status: {:?}", local_db.status()?);

    let core = alpm.register_sync_database("core")?;
    core.add_server("http://mirrors.manchester.m247.com/arch-linux/core/os/x86_64")?;
    println!(r#"core db ("{}") status: {:?}"#, core.path().display(), core.status()?);

    let extra = alpm.register_sync_database("extra")?;
    extra.add_server("http://mirrors.manchester.m247.com/arch-linux/extra/os/x86_64")?;
    let community = alpm.register_sync_database("community")?;
    community.add_server("http://mirrors.manchester.m247.com/arch-linux/community/os/x86_64")?;
    let multilib = alpm.register_sync_database("multilib")?;
    multilib.add_server("http://mirrors.manchester.m247.com/arch-linux/multilib/os/x86_64")?;
    Ok(())
}

/// Make a directory with a base installation at /tmp/alpm-test
fn make_base() {

    let base_path = Path::new(BASE_PATH);
    if base_path.is_file() {
        fs::remove_file(base_path).unwrap();
    }
    if ! base_path.exists() {
        let user = users::get_current_username().unwrap();
        let group = users::get_current_groupname().unwrap();

        fs::create_dir_all(BASE_PATH).unwrap();
        let mut cmd = Command::new("sudo");
        cmd.args(&["pacstrap", BASE_PATH, "base"]);
        if ! run_command(cmd) {
            cleanup_and_fail();
        }
        let mut chown = Command::new("sudo");
        chown.arg("chown")
            .arg("-R")
            .arg(format!("{}:{}", user, group))
            .arg(BASE_PATH);
        if ! run_command(chown) {
            cleanup_and_fail();
        }
    }
}

/// Remove tmp dir and panic
fn cleanup_and_fail() {
    assert!(BASE_PATH.starts_with("/tmp")); // don't destroy stuff
    fs::remove_dir_all(BASE_PATH).unwrap();
    panic!("make_base failed");
}

/// Run a command and panic on bad exit status
fn run_command(mut cmd: Command) -> bool {
    use std::process::Stdio;
    cmd.stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().unwrap();
    if status.success() {
        true
    } else {
        eprintln!("command {:?} failed with error code {:?}", cmd, status.code());
        false
    }
}