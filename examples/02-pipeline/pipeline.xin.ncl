# A small analysis chain:
#
#   raw ──> clean ──> stats ──> report (target)
#                └──────────────┘ (runtime dependency)
#
# Each edge is an entry in `inputs`: alias -> upstream node name. The
# alias is what the recipe sees ($XIN_INPUTS/<alias>, $XIN_INPUT_HASH_<alias>)
# and it is hashed, so renaming an alias is a real change.
let xin = import "xin.ncl" in
{
  nodes = {
    raw = {
      # in real life this would be a fetch node (example 03) or an
      # ingested dataset; a quined table keeps this example offline
      recipe = m%"
        # provenance: synthetic demo table
        printf 'sample,reads\n' >  "$XIN_PAYLOAD/samples.csv"
        printf 'wt_1,1200\n'    >> "$XIN_PAYLOAD/samples.csv"
        printf 'wt_2,1310\n'    >> "$XIN_PAYLOAD/samples.csv"
        printf 'mut_1,988\n'    >> "$XIN_PAYLOAD/samples.csv"
        printf 'blank,0\n'      >> "$XIN_PAYLOAD/samples.csv"
      "%,
    },

    clean = {
      recipe = m%"
        awk -F, 'NR == 1 || $2 > 0' \
          "$XIN_INPUTS/raw/payload/samples.csv" \
          > "$XIN_PAYLOAD/samples.csv"
      "%,
      inputs.raw = "raw",
    },

    stats = {
      recipe = m%"
        awk -F, 'NR > 1 { n++; s += $2 } END { printf "n=%d mean=%.1f\n", n, s/n }' \
          "$XIN_INPUTS/clean/payload/samples.csv" \
          > "$XIN_PAYLOAD/stats.txt"
      "%,
      inputs.clean = "clean",
    },

    report = {
      # The report *keeps using* the cleaned table after the build — a
      # runtime dependency, declared as the canonical relative link. gc
      # and downloads follow these links; undeclared use is invisible
      # and will eventually be collected out from under you.
      recipe = m%"
        cp "$XIN_INPUTS/stats/payload/stats.txt" "$XIN_PAYLOAD/report.txt"
        echo "data: see runtime-inputs/clean/payload/samples.csv" >> "$XIN_PAYLOAD/report.txt"
        ln -s "../../$XIN_INPUT_HASH_clean" "$XIN_OUT/runtime-inputs/clean"
      "%,
      inputs = { stats = "stats", clean = "clean" },
      target = true,
    },
  },
} | xin.Dag
