# Nickel is a language, not a config format: the worker nodes below —
# and the merge node's recipe — are *computed*. Change `shards` to 12
# and the whole DAG follows.
#
#            ┌> shard-0 ┐
#   dataset ─┼> shard-1 ─┼─> merged (target)
#            └> shard-2 ┘
let xin = import "xin.ncl" in
let shards = 3 in
let ids = std.array.range 0 shards in
let shard_name = fun i => "shard-%{std.string.from_number i}" in

# one worker per shard: pick every `shards`-th data line (round robin)
let workers =
  std.array.fold_left
    (fun acc i =>
      std.record.insert
        (shard_name i)
        {
          recipe = m%"
            awk -F, 'NR > 1 && (NR - 2) % %{std.string.from_number shards} == %{std.string.from_number i} { s += $2; n++ }
                     END { printf "shard %{std.string.from_number i}: n=%d sum=%d\n", n, s }' \
              "$XIN_INPUTS/dataset/payload/samples.csv" \
              > "$XIN_PAYLOAD/part.txt"
          "%,
          inputs.dataset = "dataset",
        }
        acc)
    {}
    ids
in

# the merge recipe is generated too: one cat line per shard, in order
let merge_lines =
  std.array.fold_left
    (fun acc i =>
      acc ++ "cat \"$XIN_INPUTS/%{shard_name i}/payload/part.txt\" >> \"$XIN_PAYLOAD/merged.txt\"\n")
    ""
    ids
in
let merge_inputs =
  std.array.fold_left
    (fun acc i => std.record.insert (shard_name i) (shard_name i) acc)
    {}
    ids
in

{
  nodes =
    workers
    & {
      dataset = {
        recipe = m%"
          printf 'sample,reads\nwt_1,1200\nwt_2,1310\nmut_1,988\nmut_2,1005\nwt_3,1250\nmut_3,990\n' \
            > "$XIN_PAYLOAD/samples.csv"
        "%,
      },
      merged = {
        recipe = merge_lines,
        inputs = merge_inputs,
        target = true,
      },
    },
} | xin.Dag
