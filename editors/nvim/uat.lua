




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

local seen = {}
for _, name in ipairs({ 'stopped', 'exited', 'terminated', 'process' }) do
  dap.listeners.after['event_' .. name]['uat'] = function(session, body)
    table.insert(seen, { event = name, body = body, session = session })
  end
end


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

local function frame()
  local current
  expect(vim.wait(TIMEOUT, function()
    local session = dap.session()
    current = session and session.stopped_thread_id and session.current_frame
    return current ~= nil
  end, 10), 'timed out waiting for the current frame')
  return current
end


local function request(command, arguments, session)
  local done, failure, response = false, nil, nil
  (session or dap.session()):request(command, arguments, function(err, result)
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

for _, which in ipairs({ 'child', 'parent' }) do
  local session = stops[which].session
  dap.set_session(session)
  dap.continue()
  expect(event('exited', nil, session).exitCode == 0, 'the ' .. which .. ' exited with 0')
  event('terminated', nil, session)
end



local node, npm = vim.fn.exepath('node'), vim.fn.exepath('npm')
expect(node ~= '' and npm ~= '', 'node and npm are on PATH')

local function project(files)
  local directory = vim.fn.tempname()
  vim.fn.mkdir(directory, 'p')
  for name, text in pairs(files) do
    vim.fn.writefile(vim.split(text, '\n'), directory .. '/' .. name)
  end
  return directory
end

local app = project({
  ['package.json'] = '{"name": "app", "version": "1.0.0", "scripts": {"start": "node server.js"}}',
  ['server.js'] = "const port = 8080;\nconsole.log('listening', port);\n",
})
breakpoint(app .. '/server.js', "console.log('listening'")
dap.run({
  type = 'microscope', request = 'launch', name = 'nvim-npm-start', program = npm, args = { 'start' }, cwd = app,
  env = { npm_config_cache = app .. '/npm-cache', npm_config_update_notifier = 'false' },
})
local _, served = event('stopped', function(body) return body.reason == 'breakpoint' end)
local npm_session = served.parent
expect(npm_session and npm_session.config.name == 'nvim-npm-start', 'the app\'s session is npm\'s member\'s')
dap.set_session(served)
local top = frame()
local server = app .. '/server.js'
expect(top.source and top.source.path == server, 'stopped in server.js, not ' .. vim.inspect(top.source))
expect(top.line == line_of(server, "console.log('listening'"), 'stopped at the breakpoint, not line ' .. top.line)


dap.clear_breakpoints()
dap.continue()
expect(event('exited', nil, served).exitCode == 0, 'the app exited with 0')
expect(event('exited', nil, npm_session).exitCode == 0, 'npm exited with 0')
event('terminated', nil, npm_session)




local test_body = "require('node:test')('runs', () => {\n  const answer = 42;\n  return answer;\n});\n"
local tests = project({ ['package.json'] = '{"name": "tests", "version": "1.0.0"}', ['a.test.js'] = test_body, ['b.test.js'] = test_body })
for _, name in ipairs({ 'a.test.js', 'b.test.js' }) do
  breakpoint(tests .. '/' .. name, 'return answer;')
end
dap.run({
  type = 'microscope', request = 'launch', name = 'nvim-node-test', program = node,
  args = { '--test', 'a.test.js', 'b.test.js' }, cwd = tests, follow = { mode = 'all', stop = 'process' },
})
local files, runner = {}, nil
for _ = 1, 2 do
  local body, session = event('stopped', function(body) return body.reason == 'breakpoint' end)
  runner = runner or session.parent
  expect(runner and runner.config.name == 'nvim-node-test' and session.parent == runner,
    'a file\'s node stopped, not ' .. session.config.name)




  local trace = request('stackTrace', { threadId = body.threadId }, session)
  local top = vim.tbl_filter(function(each) return each.source ~= nil end, trace.stackFrames)[1] or {}
  local file = top.source and top.source.path or ''
  expect(file:match('/[ab]%.test%.js$') ~= nil, 'stopped in a test file, not ' .. vim.inspect(top.source))
  expect(files[file] == nil, 'each file stops once: ' .. file)
  expect(top.line == line_of(file, 'return answer;'), 'stopped at the breakpoint, not line ' .. top.line)
  files[file] = session
  request('continue', { threadId = body.threadId }, session)
end
dap.clear_breakpoints()

expect(event('exited', nil, runner).exitCode == 0, 'the runner exited with 0')
event('terminated', nil, runner)



local watched = project({
  ['app.js'] = table.concat({
    "const fs = require('node:fs');",
    "const marker = __dirname + '/restarted';",
    'if (!fs.existsSync(marker)) {',
    "  fs.writeFileSync(marker, '');",
    "  const watched = [__filename, __dirname].map((path) => fs.statSync(path, { bigint: true }).ino);",
    "  const fdinfo = `/proc/${process.ppid}/fdinfo`;",
    "  const armed = (fd) => {",
    "    try {",
    "      const found = fs.readFileSync(`${fdinfo}/${fd}`, 'utf8').matchAll(/inotify wd:[^ ]+ ino:([0-9a-f]+)/g);",
    "      return [...found].some((watch) => watched.includes(BigInt(`0x${watch[1]}`)));",
    "    } catch {",
    "      return false;",
    "    }",
    "  };",
    "  const deadline = Date.now() + 60000;",
    "  const change = setInterval(() => {",
    "    if (fs.readdirSync(fdinfo).some(armed)) {",
    "      clearInterval(change);",
    "      fs.appendFileSync(__filename, '// changed\\n');",
    "    } else if (Date.now() > deadline) {",
    "      throw new Error('the watcher never watched app.js');",
    "    }",
    "  }, 10);",
    '  setInterval(() => {}, 1000);',
    '}',
    '',
  }, '\n'),
})
breakpoint(watched .. '/app.js', 'const marker =')
dap.run({ type = 'microscope', request = 'launch', name = 'nvim-node-watch', program = node, args = { '--watch', 'app.js' }, cwd = watched })
local stop, first = event('stopped', function(body) return body.reason == 'breakpoint' end)
local watcher = first.parent
expect(watcher and watcher.config.name == 'nvim-node-watch', 'the app\'s session is the watcher\'s member\'s')
request('continue', { threadId = stop.threadId }, first)



local signals, restarted = 0, nil
while restarted == nil do
  local body, session = event('stopped')
  if session == first then
    expect(body.reason ~= 'breakpoint', 'the first app stopped at the breakpoint only once')
    signals = signals + 1
    request('continue', { threadId = body.threadId }, first)
  elseif session ~= watcher then
    expect(body.reason == 'breakpoint', 'the restarted app stopped at the breakpoint, not for ' .. body.reason)
    expect(session.parent == watcher, 'the restarted app\'s session is the watcher\'s member\'s')
    restarted = { body = body, session = session }
  end
end
expect(signals == 2, 'the first app stopped twice for SIGTERM, not ' .. signals .. ' times')
local trace = request('stackTrace', { threadId = restarted.body.threadId }, restarted.session)
local top = vim.tbl_filter(function(each) return each.source ~= nil end, trace.stackFrames)[1] or {}
expect(top.source and top.source.path == watched .. '/app.js', 'stopped in app.js, not ' .. vim.inspect(top.source))
expect(top.line == 2, 'stopped at the breakpoint, not line ' .. tostring(top.line))
dap.clear_breakpoints()

request('disconnect', { terminateDebuggee = true }, watcher)
event('terminated', nil, watcher)
vim.fn.delete(app, 'rf')
vim.fn.delete(tests, 'rf')
vim.fn.delete(watched, 'rf')

print('uat: nvim-dap passed')
os.exit(0)
