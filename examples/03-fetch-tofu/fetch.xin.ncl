# A fetched input, pinned by trust-on-first-use (TOFU).
#
# `builder = "fetchurl"` nodes are the one doorway from the outside
# world into the store: their recipe is a URL (file://, http://,
# https://), the fetch runs with network access (unlike every other
# build), and the *first* fetch pins the content — from then on this
# node means "those bytes", not "whatever the URL serves today".
#
# file:// with a relative path resolves against the directory you run
# xin from — run this example from its own directory. Real projects
# would use https:// URLs or absolute paths.
let xin = import "xin.ncl" in
{
  nodes = {
    dataset = {
      builder = "fetchurl",
      recipe = "file://upstream/dataset.csv",
    },

    summary = {
      # note the m%%" ... "%% delimiters: this recipe contains `"%`,
      # which would close a plain m%" ... "% string
      recipe = m%%"
        awk -F, 'NR > 1 { n++; s += $2 } END { printf "%d samples, %d reads total\n", n, s }' \
          "$XIN_INPUTS/dataset/payload/dataset.csv" \
          > "$XIN_PAYLOAD/summary.txt"
      "%%,
      inputs.dataset = "dataset",
      target = true,
    },
  },
} | xin.Dag
