(module
  (memory (export "memory") 1)
  (func (export "acb_alloc") (param i32) (result i32) (i32.const 0))
  (func (export "acb_decide") (param i32 i32) (result i32) unreachable)
  (func (export "acb_result_len") (result i32) (i32.const 0))
)
