#![feature(path, io)]

extern crate cargo;

use std::process::{Output, Command, Stdio};
use std::path::{Path, PathBuf};
use std::fs::{File, read_link};
use std::io::{self, BufReader, BufReadExt, Cursor, Write};
use std::mem::swap;

use cargo::ops::{ExecEngine, CommandPrototype, CommandType};
use cargo::util::{self, ProcessError, ProcessBuilder};

pub struct BuildEngine {
    pub target: Option<String>,
    pub sysroot: Option<PathBuf>,
    pub emcc: Option<PathBuf>,
    pub opt: Option<PathBuf>,
    pub emit: Option<String>,
}

impl ExecEngine for BuildEngine {
    fn exec(&self, command: CommandPrototype) -> Result<(), ProcessError> {
        exec(command, false, self).map(|_| ())
    }

    fn exec_with_output(&self, command: CommandPrototype) -> Result<Output, ProcessError> {
        exec(command, true, self).map(|a| a.unwrap())
    }
}

impl BuildEngine {
    pub fn emit_needs_35(emit: &Option<String>) -> bool {
        match *emit {
            Some(ref emit) if emit.starts_with("llvm35-") || emit.starts_with("em-") => true,
            _ => false,
        }
    }
}

fn exec(mut command: CommandPrototype, with_output: bool, engine: &BuildEngine) -> Result<Option<Output>, ProcessError> {
    match command.get_type() {
        &CommandType::Rustc => (),
        _ => return do_exec(command.into_process_builder(), with_output),
    }

    // if we don't find `--crate-type bin`, returning immediatly
    let is_binary = command.get_args().windows(2)
        .find(|&args| {
            args[0].to_str() == Some("--crate-type") &&
                args[1].to_str() == Some("bin")
        }).is_some();

    // finding crate name
    let crate_name = command.get_args().windows(2)
        .filter_map(|args| {
            if args[0].to_str() == Some("--crate-name") {
                Some(args[1].to_str().unwrap().to_string())
            } else {
                None
            }
        }).next().unwrap();

    // finding out dir
    let out_dir = command.get_args().windows(2)
        .filter_map(|args| {
            if args[0].to_str() == Some("--out-dir") {
                Some(args[1].to_str().unwrap().to_string())
            } else {
                None
            }
        }).next().unwrap();

    let has_target = command.get_args()
        .iter().find(|&arg| {
            arg.to_str() == Some("--target")
        }).is_some();

    // NOTE: this is a hack, I'm not sure if there's a better way to detect this...
    // We don't want to inject --sysroot into build dependencies meant to run on the target machine
    let is_build = crate_name == "build-script-build" || (!has_target && engine.target.is_some());

    let (emit, rustc_emit, transform) = {
        if is_binary && !is_build {
            if BuildEngine::emit_needs_35(&engine.emit) {
                (engine.emit.as_ref(), Some("llvm-ir"), true)
            } else {
                (engine.emit.as_ref(), engine.emit.as_ref().map(|v| &**v), false)
            }
        } else {
            (None, None, false)
        }
    };

    if let Some(rustc_emit) = rustc_emit {
        let mut new_command = CommandPrototype::new(command.get_type().clone()).unwrap();

        for arg in command.get_args().iter().filter(|a| !a.to_str().unwrap().starts_with("--emit")) {
            new_command.arg(arg);
        }

        for (key, val) in command.get_envs().iter() {
            new_command.env(&key[..], val.as_ref().unwrap());
        }

        new_command.cwd(command.get_cwd().clone());

        new_command.arg("--emit").arg(&format!("dep-info,{}", rustc_emit));

        if transform && is_binary && !is_build {
            new_command.arg("-C").arg("lto");
        }

        swap(&mut command, &mut new_command);
    }

    if let Some(ref sysroot) = engine.sysroot {
        if !is_build {
            command.arg("--sysroot").arg(&sysroot);
        }
    }

    let output = try!(do_exec(command.into_process_builder(), with_output));
    let ll_output_file = PathBuf::new(&format!("{}/{}.ll", out_dir, crate_name));

    if transform {
        llvm35_transform(engine.opt.as_ref().map(|v| &**v).unwrap_or(&Path::new("opt")), &*ll_output_file).unwrap();
    }

    match emit {
        Some(ref emit) if emit.starts_with("em-") => {
            let extension = match &emit[..] {
                "em-html" => "html",
                "em-js" => "js",
                _ => panic!("unsupported emscripten emit type"),
            };
            let mut process = util::process(engine.emcc.as_ref().unwrap_or(&PathBuf::new("emcc"))).unwrap();
            process.arg(&ll_output_file)
                .arg("-lGL").arg("-lSDL").arg("-s").arg("USE_SDL=2")
                .arg("-o").arg(&format!("{}/{}.{}", out_dir, crate_name, extension));
            do_exec(process, with_output)
        },
        _ => Ok(output),
    }
}

fn do_exec(process: ProcessBuilder, with_output: bool) -> Result<Option<Output>, ProcessError> {
    if with_output {
        process.exec_with_output().map(|o| Some(o))
    } else {
        process.exec().map(|_| None)
    }
}

fn llvm35_transform(opt: &Path, path: &Path) -> io::Result<()> {
    // Step 1: Rewrite metadata syntax
    let input = try!(File::open(path));
    let input = BufReader::new(input);

    let mut output = Cursor::new(Vec::new());

    for line in input.lines() {
        let mut line = try!(line);
        if line.starts_with("!") {
            line = line.replace("!", "metadata !");
            line = line.replace("distinct metadata", "metadata");
            line = line["metadata ".len()..].to_string();
        }

        try!(output.write_all(line.as_bytes()));
        try!(output.write_all(&['\n' as u8]));
    }

    let source = output.into_inner();

    // Step 2: Run LLVM optimization passes to remove llvm.assume and integer overflow checks
    let opt_path = try!(read_link("/proc/self/exe"));
    let opt_path = opt_path.parent().unwrap();
    let mut opt = Command::new(opt);
    opt.arg(&format!("-load={}", opt_path.join("RemoveOverflowChecks.so").display()))
        .arg(&format!("-load={}", opt_path.join("RemoveAssume.so").display()))
        .arg("-remove-overflow-checks")
        .arg("-remove-assume")
        .arg("-globaldce")
        .arg("-S")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut opt = opt.spawn().unwrap();
    try!(opt.stdin.as_mut().unwrap().write_all(&source[..]));
    let output = opt.wait_with_output().unwrap();
    assert!(output.status.success());
    let source = output.stdout;

    let mut output = try!(File::create(path));
    try!(output.write_all(&source[..]));

    Ok(())
}
