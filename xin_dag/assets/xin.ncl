# xin DAG-definition prelude.
#
# Import it and seal your top level with the Dag contract:
#
#   let xin = import "xin.ncl" in
#   {
#     stores.primary = xin.local_store "./store",
#     nodes = {
#       base = { recipe = m%"echo -n hi > "$XIN_OUT/payload/f""% },
#       top = {
#         recipe = m%"cat "$XIN_INPUTS/base/payload/f" > "$XIN_OUT/payload/f""%,
#         inputs.base = "base",
#         target = true,
#       },
#     },
#   } | xin.Dag
#
# Evaluation exports a TOML intermediary that the Rust side parses and
# validates; this contract keeps the shape and fills the defaults, the hard
# validation (names, cycles, store references) happens after export.
{
  Node = {
    builder
      | String
      | doc "\"process\" or \"fetchurl\""
      | default
      = "process",
    recipe
      | String
      | doc "opaque build script; becomes part of the input-hash",
    target
      | Bool
      | default
      = false,
    inputs
      | { _ : String }
      | doc "local alias -> upstream node name; the alias is hashed (C1)"
      | default
      = {},
    cores
      | String
      | doc "\"one\" or \"all\" (B11)"
      | default
      = "one",
    remotes
      | Dyn
      | doc "\"all\", \"none\", { allow = [..] } or { deny = [..] } (A10)"
      | default
      = "all",
    target_store
      | String
      | doc "must name a local store (A4); defaults to the primary"
      | optional,
  },

  Dag = {
    primary
      | String
      | doc "which local store is primary; optional when only one is writeable"
      | optional,
    stores | { _ : Dyn },
    nodes | { _ : Node },
  },

  # note the argument names: a record field referring to `path` would be a
  # recursive reference to the field itself, not the function argument
  local_store = fun store_path => { type = "local", path = store_path, writeable = true },
  remote_store = fun store_url => { type = "remote", url = store_url },
}
