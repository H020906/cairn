;; Sum of squares, weighted by the length of the input.
;;
;; A stand-in for a real work unit, small enough to read in one sitting and shaped like the
;; thing Cairn exists to run: a loop over independent arithmetic, one answer out at the end.
;;
;; It reads the input only to learn how long it is, which is enough to make two volunteers
;; given different inputs compute different answers — the situation `cairn-worker dispute`
;; needs. Reading it at the *end* means the two executions stay identical until then, which is
;; the expensive shape for a dispute and therefore the interesting one to demonstrate.
(module
  (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
  (import "cairn" "output" (func $output (param i32 i32)))

  ;; One page, and a declared maximum. Cairn requires the maximum: memory growth that could
  ;; fail differently on different machines would be a source of disagreement.
  (memory (export "memory") 1 1)

  (func (export "cairn_run")
    (local $i i32)
    (local $acc i64)
    (local $n i32)

    (block $done
      (loop $again
        (br_if $done (i32.ge_u (local.get $i) (i32.const 50000)))
        (local.set $acc
          (i64.add (local.get $acc)
                   (i64.mul (i64.extend_i32_u (local.get $i))
                            (i64.extend_i32_u (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))

    ;; Asking for zero bytes still reports how many are available, so this learns the input's
    ;; length without needing anywhere to put it.
    (local.set $n (call $input (i32.const 0) (i32.const 0)))
    (local.set $acc (i64.add (local.get $acc) (i64.extend_i32_u (local.get $n))))

    (i64.store (i32.const 0) (local.get $acc))
    (call $output (i32.const 0) (i32.const 8))))
