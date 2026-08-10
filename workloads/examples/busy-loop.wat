;; A work unit heavy enough to measure a volunteer with.
;;
;; `sum-of-squares.wat` is the one to read: it is the same shape and small enough to follow. This
;; one exists for a different job, and the difference is only the loop bound.
;;
;; Measuring how many units a machine gets through per second is meaningless if a unit is over in
;; a hundred microseconds — at that size a run measures wasmtime's compiler, the HTTP round trip
;; and the coordinator's accept loop, in roughly that order, and barely touches the thing being
;; claimed. Compilation is paid once per unit and is a fixed few milliseconds; execution here is
;; a few hundred, so the ratio is about a hundred to one and what gets measured is execution.
;;
;; It is also deliberately the *easy* case for parallelism: one page of memory, no growth, no
;; allocation, arithmetic in registers. A workload like this scales until the cores run out. A
;; memory-heavy one stops scaling earlier, at whatever the machine's memory bandwidth is, and
;; that number is a property of the machine rather than of Cairn.
;;
;; # Why the loop stops where it does, which is a rule for every workload author
;;
;; 150,000,000 iterations is about 3.1 billion instructions, and the ceiling it is under is
;; `Limits::default().fuel`, which is 2^32. That limit belongs to Cairn's *interpreter*, so a
;; workload can exceed it and never notice: the honest path runs on wasmtime, which has no such
;; limit, and every unit completes normally. What breaks is the dispute path — the replay traps
;; on fuel, the party cannot answer, and it loses an argument it was winning.
;;
;; So: **a unit long enough to run but too long to replay is a unit that cannot be defended.**
;; A workload author who never opens the dispute path will not find this out from a test run.
;;
;;   cargo run --release -p cairn-coordinator -- workloads/examples/busy-loop.wat <inputs...>
;;   cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080 --idle-exit 4
;;
;; Add `--jobs 1` to the volunteer for the single-core baseline to compare against.
(module
  (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
  (import "cairn" "output" (func $output (param i32 i32)))

  ;; One page, and a declared maximum, exactly as in sum-of-squares. Cairn requires the maximum,
  ;; and a volunteer budgets its concurrency against it — see worker-native/src/capacity.rs.
  (memory (export "memory") 1 1)

  (func (export "cairn_run")
    (local $i i32)
    (local $acc i64)
    (local $n i32)

    (block $done
      (loop $again
        (br_if $done (i32.ge_u (local.get $i) (i32.const 150000000)))
        (local.set $acc
          (i64.add (local.get $acc)
                   (i64.mul (i64.extend_i32_u (local.get $i))
                            (i64.extend_i32_u (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))

    ;; The input's length and nothing else, so that two units with different inputs produce
    ;; different answers and a replicated unit is a real comparison.
    (local.set $n (call $input (i32.const 0) (i32.const 0)))
    (local.set $acc (i64.add (local.get $acc) (i64.extend_i32_u (local.get $n))))

    (i64.store (i32.const 0) (local.get $acc))
    (call $output (i32.const 0) (i32.const 8))))
