-- Operations dwm runs against a Rex server with `rex do - --args JSON`.
--
-- dwm embeds this file and pipes it to `rex do` on stdin. `rex.args.op` picks
-- the operation, and the value returned is printed as JSON for dwm to parse.
-- Keep the shapes in sync with the serde types in src/rex.rs.

local TERMINAL = "com.superlogical.terminal"
local SHELL = "com.superlogical.terminal.shell"
local args = rex.args or {}

-- Ask the connected Rex app to show a session. This needs a client that
-- accepts actions from scripts ("Remote Control" in Rex's settings), so it can
-- fail even when everything else works. Returns true, or false and a reason.
local function show_session(session_id, window_id)
  local ok, err = pcall(rex.client.queue, "session.select", {
    session_id = session_id,
    window_id = window_id,
  })
  if ok then
    return true
  end
  return false, tostring(err)
end

-- Every terminal pane on the server, with the directory its foreground process
-- is in and the OSC 7501 status records programs in it have reported.
local function scan()
  local panes = {}
  local listed, err = rex.call("session.list")
  if listed == nil then
    error("listing sessions: " .. tostring(err), 0)
  end
  for _, session in ipairs(listed.sessions or {}) do
    -- A script can only read the sessions it is attached to.
    rex.call("session.attach", { session_id = session.session_id })
    local blocks = rex.call("session.list_blocks", { session_id = session.session_id })
    for _, block in ipairs((blocks and blocks.blocks) or {}) do
      if block.creator_name == TERMINAL then
        local target = { session_id = session.session_id, block_id = block.block_id }
        local process = rex.block.call(TERMINAL, "process", target) or {}
        local status = rex.block.call(TERMINAL, "program_status", target) or {}
        local fg = process.foreground or {}
        local shell = (process.ancestors or {})[1] or {}
        table.insert(panes, {
          session_id = session.session_id,
          session_label = session.label,
          block_id = block.block_id,
          window_id = block.window_id,
          cwd = fg.cwd or shell.cwd,
          command = fg.argv0 or fg.name,
          records = status.records,
        })
      end
    end
  end
  return { panes = panes }
end

local function focus()
  rex.call("session.attach", { session_id = args.session_id })
  rex.call("session.focus_block", { session_id = args.session_id, block_id = args.block_id })
  local shown, reason = show_session(args.session_id, args.window_id)
  return { shown = shown, reason = reason }
end

local function open()
  -- Find out whether the app will show a new session before making one, by
  -- re-selecting the session this script runs in (which the person is
  -- already looking at). Without that, `always_create` decides.
  if rex.session_id ~= nil and not args.always_create then
    local can_show, reason = show_session(rex.session_id)
    if not can_show then
      return { created = false, shown = false, reason = reason }
    end
  end

  local options = { cwd = args.cwd }
  if args.initial_input ~= nil then
    options.initial_input = args.initial_input
  end
  local created, err = rex.call("session.create", {
    label = args.label,
    initial_windows = {
      { layout = rex.layout.block({ flavor = SHELL, options = options }) },
    },
  })
  if created == nil then
    error("creating session: " .. tostring(err), 0)
  end
  local session_id = created.session_id
  local shown, reason = show_session(session_id)
  return { created = true, session_id = session_id, shown = shown, reason = reason }
end

local function close()
  rex.call("session.attach", { session_id = args.session_id })
  local _, err = rex.call("block.close", { session_id = args.session_id, block_id = args.block_id })
  if err ~= nil then
    error("closing pane: " .. tostring(err), 0)
  end
  -- Closing a session's last pane leaves an empty session behind; remove it.
  local left = rex.call("session.list_blocks", { session_id = args.session_id })
  if left ~= nil and #(left.blocks or {}) == 0 then
    rex.call("session.destroy", { session_id = args.session_id })
  end
  return { ok = true }
end

local function rename()
  rex.call("session.attach", { session_id = args.session_id })
  local _, err = rex.call("session.set_label", { session_id = args.session_id, label = args.label })
  if err ~= nil then
    error("renaming session: " .. tostring(err), 0)
  end
  return { ok = true }
end

local ops = { scan = scan, focus = focus, open = open, close = close, rename = rename }
local op = ops[args.op]
if op == nil then
  error("unknown op: " .. tostring(args.op), 0)
end
return op()
