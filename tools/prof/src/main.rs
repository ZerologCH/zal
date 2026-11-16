//! Sampling profile of a ZAL run (SIGPROF, no root needed).
//! Usage: zal-prof <file.zal> <entry> <args.json> <mock.json> <policy.json> [iters]
use zal_vm::host::{MockHost, PolicyBroker};
use zal_vm::Vm;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = std::path::PathBuf::from(&a[1]).parent().unwrap().to_path_buf();
    let p = zal_compiler::compile_file(std::path::Path::new(&a[1]), &[root, "zal-src".into()]).unwrap_or_else(|d| panic!("{}", d[0]));
    let (_, f) = p.function(&a[2]).unwrap();
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&a[3]).unwrap()).unwrap();
    let args: Vec<_> = j.as_array().unwrap().iter().zip(&f.params).map(|(j, (_, t))| zal_vm::value::from_json(j, t, &p).unwrap()).collect();
    let mock = std::fs::read_to_string(&a[4]).unwrap();
    let pol = std::fs::read_to_string(&a[5]).unwrap();
    let iters: usize = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(50);
    let guard = pprof::ProfilerGuardBuilder::default().frequency(2000).build().unwrap();
    for _ in 0..iters {
        let mut vm = Vm::new(&p, MockHost::from_json(&mock).unwrap(), PolicyBroker::from_json(&pol).unwrap());
        vm.run(&a[2], args.clone()).unwrap();
    }
    let report = guard.report().build().unwrap();
    // Self time per function (leaf frame of each sample).
    let mut selfs: std::collections::HashMap<String, isize> = Default::default();
    let mut total = 0isize;
    for (frames, count) in report.data.iter() {
        total += *count;
        // Innermost frame that lies in ZAL source, as file:line.
        // Innermost non-inlined function (last symbol of the leaf frame).
        let name = frames.frames.first().and_then(|f| f.last()).map(|s| s.name()).unwrap_or_default();
        let short: String = name.replace("zal_vm::", "").replace("<zal_vm::Vm<zal_vm::host::MockHost, zal_vm::host::PolicyBroker>>::", "Vm::");
        let chain = vec![short];
        *selfs.entry(chain.join(" <- ")).or_default() += *count;
    }
    let mut v: Vec<_> = selfs.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    println!("samples: {total}");
    for (n, c) in v.iter().take(30) {
        let short: String = n.chars().take(110).collect();
        println!("{:5.1}%  {}", 100.0 * *c as f64 / total as f64, short);
    }
    if let Ok(file) = std::fs::File::create("target/zal-prof.svg") {
        let _ = report.flamegraph(file);
    }
}
