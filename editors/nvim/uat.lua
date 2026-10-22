-- User acceptance test: nvim-dap drives `microscope dap` from a headless
-- Neovim, as a user would, and the adapter records each session's traffic.
--
-- nvim --headless --clean -l editors/nvim/uat.lua NVIM_DAP ROOT RECORDING_DIRECTORY

local nvim_dap, root, record = arg[1], arg[2], arg[3]
vim.opt.runtimepath:prepend(nvim_dap)
local dap = require('dap')

local TIMEOUT = 15000
local fixture = function(name) return root .. '/build/test-programs/' .. name end
local source = function(name) return root .. '/tests/fixtures/c/' .. name end
vim.fn.mkdir(record, 'p')

local function fail(message)
  io.stderr:write('uat: ' .. message .. '\n')
  os.exit(1)
end

local function expect(condition, message)
  if not condition then
    fail(message)
  end
end

-- The adapter's events since the last wait, by kind, with their sessions.
local seen = {}
for _, name in ipairs({ 'stopped', 'exited', 'terminated', 'process' }) do
  dap.listeners.after['event_' .. name]['uat'] = function(session, body)
    table.insert(seen, { event = name, body = body, session = session })
  end
end

--- Waits for an event the predicate accepts, consuming those before it;
--- with a session, only that session's. Returns its body and session.
local function event(name, predicate, session)
  local found, from
  local ok = vim.wait(TIMEOUT, function()
    local index = 1
    while index <= #seen do
      local next = seen[index]
      if session == nil or next.session == session then
        table.remove(seen, index)
        if next.event == name and (predicate == nil or predicate(next.body)) then
          found, from = next.body, next.session
          return true
        end
      else
        index = index + 1
      end
    end
    return false
  end, 10)
  expect(ok, 'timed out waiting for a ' .. name .. ' event')
  return found, from
end

--- Waits until nvim-dap has fetched the stopped thread's frames.
local function frame()
  local current
  expect(vim.wait(TIMEOUT, function()
    local session = dap.session()
    current = session and session.stopped_thread_id and session.current_frame
    return current ~= nil
  end, 10), 'timed out waiting for the current frame')
  return current
end

--- Sends a request and waits for its response.
local function request(command, arguments)
  local done, failure, response = false, nil, nil
  dap.session():request(command, arguments, function(err, result)
    done, failure, response = true, err, result
  end)
  expect(vim.wait(TIMEOUT, function() return done end, 10), 'timed out waiting for ' .. command)
  expect(failure == nil, command .. ' failed: ' .. vim.inspect(failure))
  return response
end

local function line_of(file, marker)
  for number, line in ipairs(vim.fn.readfile(file)) do
    if line:find(marker, 1, true) then
      return number
    end
  end
  fail(marker .. ' is not in ' .. file)
end

local function breakpoint(file, marker)
  vim.cmd.edit(file)
  vim.api.nvim_win_set_cursor(0, { line_of(file, marker), 0 })
  dap.toggle_breakpoint()
end

local function adapter(name)
  dap.adapters.microscope = {
    type = 'executable',
    command = 'microscope',
    args = { 'dap', '--log', record .. '/' .. name .. '.log' },
  }
end

-- Breakpoints, stepping, evaluation, and the program's exit.
adapter('nvim-launch')
breakpoint(source('basic.c'), 'return microscope_value;')
dap.run({ type = 'microscope', request = 'launch', name = 'nvim-launch', program = fixture('basic'), cwd = root })
expect(event('stopped').reason == 'breakpoint', 'stopped at the breakpoint')
local top = frame()
expect(top.name == 'breakpoint_target', 'stopped in breakpoint_target, not ' .. top.name)
expect(top.line == line_of(source('basic.c'), 'return microscope_value;'), 'stopped at the line')
local value = request('evaluate', { expression = 'microscope_value', frameId = top.id, context = 'hover' })
expect(value.result == '1234605616436508552', 'evaluated ' .. vim.inspect(value))
dap.step_out()
expect(event('stopped').reason == 'step', 'stepped out')
expect(frame().name == 'main', 'returned to main')
dap.step_over()
expect(event('stopped').reason == 'step', 'stepped over')
frame()
dap.clear_breakpoints()
dap.continue()
expect(event('exited').exitCode == 0, 'the program exited with 0')
event('terminated')

-- A program in nvim-dap's terminal, which owns its input and output.
adapter('nvim-terminal')
breakpoint(source('process-environment.c'), 'if (fgets(line')
dap.run({
  type = 'microscope', request = 'launch', name = 'nvim-terminal', program = fixture('process-environment'),
  args = { 'one' }, console = 'integratedTerminal', cwd = root,
})
expect(event('process').startMethod == 'launch', 'launched')
expect(event('stopped').reason == 'breakpoint', 'stopped in the terminal program')
expect(frame().name == 'main', 'stopped in main')
local terminal = vim.tbl_filter(function(buffer)
  return vim.bo[buffer].buftype == 'terminal'
end, vim.api.nvim_list_bufs())[1]
expect(terminal ~= nil, 'nvim-dap opened a terminal')
dap.clear_breakpoints()
dap.continue()
vim.api.nvim_chan_send(vim.bo[terminal].channel, 'typed\n')
expect(event('exited').exitCode == 2, 'the terminal program exited with its argument count')
event('terminated')
local lines = table.concat(vim.api.nvim_buf_get_lines(terminal, 0, -1, false), '\n')
expect(lines:find('argument 1: one', 1, true) ~= nil, 'the terminal shows the output: ' .. lines)

-- With follow, nvim-dap starts a session for a forked process at the
-- adapter's request, which the adapter function relays back to the same
-- adapter with microscope dap --connect, as docs/editors.md shows. The child stops at the breakpoint after
-- its fork, having run nothing untraced, and each session ends as its process
-- does. Process stops keep the two stops independent of which comes first.
dap.adapters.microscope = function(callback, config)
  local member = config.__microscopeMember
  if member then
    callback({ type = 'executable', command = 'microscope', args = { 'dap', '--connect', member.socket } })
  else
    callback({ type = 'executable', command = 'microscope', args = { 'dap', '--log', record .. '/' .. config.name .. '.log' } })
  end
end
breakpoint(source('fork.c'), 'work_done += 1;')
dap.run({
  type = 'microscope', request = 'launch', name = 'nvim-fork', program = fixture('fork'), cwd = root,
  follow = { mode = 'all', stop = 'process' },
})
local stops = {}
for _ = 1, 2 do
  local body, session = event('stopped')
  expect(body.reason == 'breakpoint', 'stopped at the breakpoint, not ' .. body.reason)
  stops[session.parent and 'child' or 'parent'] = { session = session, thread = body.threadId }
end
expect(stops.parent and stops.child, 'both processes stopped')
expect(stops.child.session.parent == stops.parent.session, 'the child session is the parent session\'s')
expect(stops.child.session.config.request == 'attach', 'the child session attached')
for _, which in ipairs({ 'child', 'parent' }) do
  local stop = stops[which]
  dap.set_session(stop.session)
  local trace = request('stackTrace', { threadId = stop.thread, levels = 1 })
  expect(trace.stackFrames[1].name == 'shared_work', which .. ' stopped in shared_work')
end
dap.clear_breakpoints()
-- The child runs to its end, and then its parent, which waited for it.
for _, which in ipairs({ 'child', 'parent' }) do
  local session = stops[which].session
  dap.set_session(session)
  dap.continue()
  expect(event('exited', nil, session).exitCode == 0, 'the ' .. which .. ' exited with 0')
  event('terminated', nil, session)
end

print('uat: nvim-dap passed')
os.exit(0)
