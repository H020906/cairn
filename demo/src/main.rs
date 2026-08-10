//! A whole Cairn grid in one process, running slowly enough that a person can watch it.
//!
//! ```bash
//! cargo run --release -p cairn-demo
//! ```
//!
//! Then open the printed address. The page narrates what is happening, in Chinese, because the
//! person this was built for cannot read the rest of the repository — and a project nobody can
//! see working is a project nobody understands.
//!
//! # What is real here and what is staged
//!
//! Being exact about this matters more than the demo looking good.
//!
//! **Real:** the coordinator is [`cairn_coordinator::grid::Grid`], unmodified. The volunteers
//! answer challenges with [`cairn_runtime::dispute::answer`], the same function the native
//! worker calls. The bisection is [`cairn_runtime::dispute::resolve`]. The adjudication executes
//! one instruction from a state witness a party actually supplied. The page reads `/api/status`
//! and `/api/disputes`, which is what any client sees.
//!
//! **Staged:** three things, all of them stated on the page itself.
//!
//! 1. **The pacing.** A dispute over a million instructions settles in well under a second in
//!    one process, which is too fast to see. Every answer waits [`Pace`] first. The computation
//!    is not slowed — only the conversation.
//! 2. **The replication rate is 100%.** Every unit is double-checked, so both outcomes are
//!    visible. A real network sets this near 10%, and the other 90% — accepted after a *single*
//!    execution — is where the saving actually comes from.
//! 3. **Everyone is in one process.** A real volunteer is a browser tab running the workload on
//!    the engine it already has; here both volunteers run Cairn's own interpreter, because a
//!    demo that needs three terminals is a demo nobody starts.
//!
//! **The liar is a real liar.** It returns a wrong answer *and* corrupts every state root it
//! claims from a chosen step onwards — because a party that lies only once is not cheating, it
//! is merely wrong: a replay is deterministic, so answering challenges honestly reproduces the
//! truth and agrees with everybody. See `docs/adr/0011`.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cairn_coordinator::api;
use cairn_coordinator::dispute::{answer_honestly, Answer, Question};
use cairn_coordinator::grid::{Grid, Submission};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};

/// How long a volunteer waits before answering, so the argument is watchable.
///
/// Not a simulation of network latency and not presented as one — the page says the pacing is
/// artificial. Twenty rounds at this pace is about half a minute, which is long enough to follow
/// and short enough to watch twice.
type Pace = Duration;

const DEFAULT_PACE: Pace = Duration::from_millis(700);

/// The step from which the dishonest volunteer starts corrupting its claims.
///
/// Deep inside a 1,050,030-instruction execution, so the bisection has real work to do and the
/// bracket visibly narrows for twenty rounds rather than three.
const LIES_FROM: u64 = 500_000;

/// Which unit the dishonest volunteer lies about.
///
/// It answers the first two honestly, so the page shows the ordinary outcome — two independent
/// executions agreeing — before it shows the interesting one. A cheat that cheats every time is
/// not the case the economics are about.
const LIES_ABOUT_UNIT: usize = 2;

const USAGE: &str = "\
cairn-demo — 把整个 Cairn 网格跑在一个进程里，慢到人能看清

用法
    cairn-demo [--bind 地址] [--pace 毫秒]

    --bind   默认 127.0.0.1:8080
    --pace   每次回答前等待的毫秒数，默认 700。调成 0 就是全速。
";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut bind = "127.0.0.1:8080".to_owned();
    let mut pace = DEFAULT_PACE;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--bind" => bind = rest.next().ok_or("--bind 需要一个地址")?.clone(),
            "--pace" => {
                let ms: u64 = rest
                    .next()
                    .ok_or("--pace 需要一个毫秒数")?
                    .parse()
                    .map_err(|_| "--pace 需要一个数字")?;
                pace = Duration::from_millis(ms);
            }
            other => return Err(format!("不认识的参数 {other}")),
        }
    }

    let (workload, web_root) = locate()?;
    let source = std::fs::read(&workload).map_err(|e| format!("读不到 {workload}: {e}"))?;

    // Replication at 100% so every unit is double-checked and both outcomes are visible. A real
    // network runs this near 10% and the page says so.
    let mut grid = Grid::new()
        .with_replication(100)
        .with_patience(Duration::from_secs(120));
    let id = grid.register("sum-of-squares", &source)?;

    // Three units with different inputs, so their answers differ and the page is not showing the
    // same eight bytes three times.
    for input in [b"alpha".as_slice(), b"beta".as_slice(), b"gamma".as_slice()] {
        grid.submit(&id, input.to_vec())?;
    }

    let disputable = Arc::clone(&grid.workload(&id).ok_or("刚注册的负载不见了")?.disputable);
    let grid = Arc::new(Mutex::new(grid));

    println!("Cairn 演示 → http://{bind}");
    println!("  三个任务，两个志愿者，其中一个会在第 {LIES_FROM} 步之后开始说谎。");
    println!("  每次回答前停 {} 毫秒，好让你看清。", pace.as_millis());
    println!();

    for (name, lies) in [("诚实志愿者", false), ("说谎志愿者", true)] {
        spawn_volunteer(
            &grid,
            name,
            Arc::clone(&disputable),
            lies.then_some(LIES_FROM),
            pace,
        );
    }

    api::serve(grid, &bind, Some(&web_root))
}

/// Find the workload and the page, whether run from the repository root or from `demo/`.
fn locate() -> Result<(String, String), String> {
    for (workload, web) in [
        ("workloads/examples/sum-of-squares.wat", "demo/web"),
        ("../workloads/examples/sum-of-squares.wat", "web"),
    ] {
        if std::path::Path::new(workload).is_file() && std::path::Path::new(web).is_dir() {
            return Ok((workload.to_owned(), web.to_owned()));
        }
    }
    Err("找不到 workloads/examples/sum-of-squares.wat 或 demo/web/，请在仓库根目录运行".to_owned())
}

/// One volunteer: takes units, runs them, and answers challenges about them afterwards.
///
/// A faithful miniature of `worker-native`'s `volunteer` command, minus the HTTP — it reaches the
/// same [`Grid`] through the same methods the handlers call.
fn spawn_volunteer(
    grid: &Arc<Mutex<Grid>>,
    name: &str,
    disputable: Arc<Vec<u8>>,
    lies_from: Option<u64>,
    pace: Pace,
) {
    let grid = Arc::clone(grid);
    let name = name.to_owned();

    drop(thread::spawn(move || {
        let Ok(image) = image::decode(&disputable) else {
            eprintln!("{name}: 争议用的模块解不开");
            return;
        };
        let mut done: Option<u64> = None;

        loop {
            // A dispute holds a unit and two volunteers; answering comes before taking new work.
            let outstanding = {
                let Ok(grid) = grid.lock() else { return };
                grid.dispute_for(&name).and_then(|(_, dispute)| {
                    let desk = Arc::clone(dispute.desk_for(&name)?);
                    let (token, question) = desk.pending()?;
                    (done != Some(token)).then_some((desk, token, question))
                })
            };

            if let Some((desk, token, question)) = outstanding {
                // The wait is the whole reason this demo is watchable. It slows the
                // *conversation*, never the computation — the replay below runs at full speed.
                thread::sleep(pace);

                let Ok(honest) = answer_honestly(
                    &image,
                    &input_for(&grid, &name),
                    Limits::default(),
                    question,
                ) else {
                    continue;
                };
                if desk.reply(token, distort(honest, question, lies_from)) {
                    done = Some(token);
                }
                continue;
            }

            if take_a_unit(&grid, &name, lies_from.is_some(), pace).is_none() {
                thread::sleep(Duration::from_millis(120));
            }
        }
    }));
}

/// The input of whichever unit this worker is currently disputing.
///
/// The coordinator holds it authoritatively; a party replaying a different input would produce
/// well-formed answers to a different question.
fn input_for(grid: &Arc<Mutex<Grid>>, worker: &str) -> Vec<u8> {
    let Ok(grid) = grid.lock() else {
        return Vec::new();
    };
    grid.dispute_for(worker)
        .and_then(|(_, d)| grid.unit(d.unit))
        .map(|unit| unit.input.clone())
        .unwrap_or_default()
}

/// Lease a unit, run it, and report. `None` when there is nothing to do.
fn take_a_unit(grid: &Arc<Mutex<Grid>>, name: &str, dishonest: bool, pace: Pace) -> Option<()> {
    let (assignment, module) = {
        let mut locked = grid.lock().ok()?;
        let assignment = locked.lease(name, Instant::now())?;
        let module = locked.workload(&assignment.workload)?.module.clone();
        (assignment, module)
    };

    // Paced so the first two units do not flash past before the page has drawn them.
    thread::sleep(pace);

    let image = image::decode(&module).ok()?;
    let mut machine = Machine::new(&image, assignment.input.clone(), Limits::default()).ok()?;
    let trace = machine.run().ok()?;

    // A liar has to lie twice, and this is the first lie: a wrong answer is what starts a
    // dispute. The second lie — corrupted state roots — is in `distort`, and without it this
    // party would simply be *wrong* rather than caught.
    let output = if dishonest && assignment.unit == LIES_ABOUT_UNIT {
        vec![0xde, 0xad, 0xbe, 0xef]
    } else {
        trace.output
    };

    let mut locked = grid.lock().ok()?;
    let _ = locked.submit_result(
        assignment.unit,
        Submission {
            worker: name.to_owned(),
            output,
            fuel: Some(trace.fuel.get()),
            bisects: true,
        },
    );
    Some(())
}

/// Corrupt a claimed state root, if this volunteer is the dishonest one and the step is past the
/// point it started lying.
fn distort(honest: Answer, question: Question, lies_from: Option<u64>) -> Answer {
    let (Some(from), Question::Root { step }, Answer::Root(root)) = (lies_from, question, &honest)
    else {
        return honest;
    };
    if step < from {
        return honest;
    }
    Answer::Root(root.map(|mut corrupted| {
        if let Some(first) = corrupted.first_mut() {
            *first ^= 0xff;
        }
        corrupted
    }))
}
