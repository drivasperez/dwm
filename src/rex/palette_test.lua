-- Run from the repository root: lua src/rex/palette_test.lua
local actions, prompts, calls, listing, in_repo, result, session, layer, selected
local ctx = { session_id = "source", block_id = "focused" }

local function reset()
  prompts, calls, listing = {}, {}, {}
  in_repo = true
  result = { ok = true, stdout = "/worktree/generated-name\n", stderr = "" }
  session, layer, selected = nil, nil, nil
end

local function next_prompt()
  return table.remove(prompts, 1)
end

reset()
actions = {}
rex = {
  action = function(action) actions[action.name] = action.run end,
  block = { call = function() return { foreground = { cwd = "/focused/subdir" } } end },
  ui = { input = next_prompt, pick = function(opts)
    calls.pick = opts
    return next_prompt()
  end },
  exec = function(argv, opts)
    if argv[1] == "/bin/sh" then return { ok = in_repo } end
    if argv[2] == "status" then return { ok = true, stdout = "json" } end
    calls.new = { argv = argv, opts = opts }
    return result
  end,
  json = { decode = function() return { workspaces = listing } end },
  layout = { block = function(block) return block end },
  call = function(method, args)
    assert(method == "session.create")
    session = args
    return { session_id = "new" }
  end,
  client = { queue = function(method, args)
    assert(method == "session.select")
    selected = args.session_id
  end },
  session = { new_layer = function(args) layer = args end },
}
dofile("src/rex/palette.lua")

-- Blank inputs provision a generated workspace and open a plain shell.
prompts = { "", "" }
actions.dwm_new(ctx)
assert(calls.new.opts.cwd == "/focused/subdir")
assert(table.concat(calls.new.argv, " ") == "@DWM@ new --here")
assert(session.label == "generated-name" and selected == "new")
assert(session.initial_windows[1].layout.options.cwd == "/worktree/generated-name")
assert(session.initial_windows[1].layout.options.initial_input == nil)

-- Names travel as one argument; only the command goes to the new shell.
reset()
prompts = { "fix login", 'pi "fix login"' }
actions.dwm_new(ctx)
assert(calls.new.argv[4] == "fix login")
assert(session.initial_windows[1].layout.options.initial_input == 'pi "fix login"\r')

-- Cancel, invalid option-like names and failed creation must not open a session.
reset()
prompts = { "cancelled-at-command" }
actions.dwm_new(ctx)
assert(not calls.new and not session)
prompts = { "--help" }
assert(not pcall(actions.dwm_new, ctx) and not calls.new and not session)
prompts = { "name", "" }
result = { ok = false, stderr = "workspace already exists" }
assert(not pcall(actions.dwm_new, ctx) and not session)

-- Outside a repo, ask for a main checkout and honour cancellation.
reset()
in_repo = false
listing = {
  { is_main = false, path = "/other.dwm/worktrees/agent", name = "agent" },
  { is_main = true, path = "/other", repo = "other" },
}
actions.dwm_prune(ctx)
assert(not layer and #calls.pick.items == 1 and calls.pick.items[1].id == "/other")
prompts = { "/other" }
actions.dwm_prune(ctx)
assert(layer.session_id == "source" and layer.layout.options.cwd == "/other")
local argv = layer.layout.options.command
assert(argv[3] == "DWM_REX=1" and argv[#argv] == "@DWM@")

-- Exercise the actual confirmation script with a fake dwm binary.
local shell, script = argv[5], argv[7]
local function quote(s) return "'" .. s:gsub("'", "'\\''") .. "'" end
local fake = os.tmpname()
local file = assert(io.open(fake, "w"))
file:write('#!/bin/sh\nprintf "<%s>\\n" "$*"\n[ "${FAIL_PREVIEW:-}" != 1 ]\n')
file:close()
os.execute("chmod +x " .. quote(fake))
local function prune(input, fail_preview)
  local pipe = assert(io.popen("printf '%s' " .. quote(input) .. " | FAIL_PREVIEW="
    .. (fail_preview and "1" or "0") .. " " .. shell .. " -c " .. quote(script) .. " test " .. quote(fake)))
  local output = pipe:read("*a")
  pipe:close()
  return output
end
local declined, accepted, failed = prune("\n"), prune("y\n\n"), prune("y\n\n", true)
os.remove(fake)
assert(declined:find("<prune>", 1, true) and not declined:find("<prune --yes>", 1, true))
assert(accepted:find("<prune>", 1, true) < accepted:find("<prune --yes>", 1, true))
assert(not failed:find("[y/N]", 1, true) and not failed:find("<prune --yes>", 1, true))
print("Rex palette tests passed")
