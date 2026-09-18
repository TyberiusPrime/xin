# The smallest possible xin DAG: one node, one output file.
#
# `xin.Dag` is the contract from the shipped prelude — it fills in the
# defaults (builder = "process", cores = "one", ...) and checks the shape
# before the Rust side ever sees it.
let xin = import "xin.ncl" in
{
  nodes = {
    hello = {
      recipe = m%"
        echo "hello, xin" > "$XIN_PAYLOAD/greeting.txt"
      "%,
      target = true,
    },
  },
} | xin.Dag
