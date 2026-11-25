--- A JSON-RPC 2.0 client for `dbear-cli serve` over stdio: one JSON message per line.
--- No UI here, so other Lua code can drive the server too. The process starts on the first
--- request and again after it exits; requests in flight when it dies fail with code -32099.

local M = {}
M.__index = M

--- Error code for requests the client fails itself (the server exited, or couldn't start).
M.EXITED = -32099

--- @class dbear.rpc.Options
--- @field cmd string[] e.g. { "dbear-cli", "serve" }
--- @field env? table<string,string> extra environment (merged into the current one)
--- @field schedule? fun(f: function) runs callbacks; default `vim.schedule` (callbacks may call the API)
--- @field on_notification? fun(method: string, params: any)
--- @field on_exit? fun(code: integer, stderr: string)

--- @param opts dbear.rpc.Options
function M.new(opts)
  return setmetatable({ opts = opts, next_id = 0, pending = {}, proc = nil, partial = "", stderr = "" }, M)
end

local function decode(line)
  -- Keep JSON null as vim.NIL so rows keep their length.
  local ok, msg = pcall(vim.json.decode, line)
  return ok and msg or nil
end

function M:_dispatch(line)
  local msg = decode(line)
  if type(msg) ~= "table" then
    return
  end
  if msg.method then -- a notification from the server
    local handler = self.opts.on_notification
    if handler then
      (self.opts.schedule or vim.schedule)(function() handler(msg.method, msg.params) end)
    end
    return
  end
  local req = self.pending[msg.id]
  if not req then
    return
  end
  self.pending[msg.id] = nil
  local err = msg.error ~= nil and msg.error ~= vim.NIL and msg.error or nil
  self:_answer(req, err, msg.result)
end

function M:_answer(req, err, result)
  if req.fast then
    req.cb(err, result)
  else
    (self.opts.schedule or vim.schedule)(function() req.cb(err, result) end)
  end
end

function M:_on_stdout(_, data)
  if not data then
    return
  end
  local text = self.partial .. data
  local start = 1
  while true do
    local nl = text:find("\n", start, true)
    if not nl then
      break
    end
    self:_dispatch(text:sub(start, nl - 1))
    start = nl + 1
  end
  self.partial = text:sub(start)
end

function M:_on_exit(proc, code)
  if self.proc ~= proc then
    return
  end
  self.proc, self.partial = nil, ""
  local pending, stderr = self.pending, self.stderr
  self.pending = {}
  for _, req in pairs(pending) do
    self:_answer(req, { code = M.EXITED, message = "dbear-cli exited (" .. code .. ")" .. (stderr ~= "" and ": " .. stderr or "") })
  end
  if self.opts.on_exit then
    (self.opts.schedule or vim.schedule)(function() self.opts.on_exit(code, stderr) end)
  end
end

--- Starts the server unless it runs. Returns nil and a message when it can't.
function M:start()
  if self.proc then
    return self.proc
  end
  self.stderr = ""
  local proc
  local ok, err = pcall(function()
    proc = vim.system(self.opts.cmd, {
      stdin = true,
      env = self.opts.env,
      stdout = function(e, data) self:_on_stdout(e, data) end,
      stderr = function(_, data)
        if data then -- keep the tail for error messages
          self.stderr = (self.stderr .. data):sub(-2000):gsub("%s+$", "")
        end
      end,
    }, function(out) self:_on_exit(proc, out.code) end)
  end)
  if not ok then
    return nil, "can't start " .. tostring(self.opts.cmd[1]) .. ": " .. tostring(err)
  end
  self.proc = proc
  return proc
end

--- Sends a request; `cb(err, result)` gets the JSON-RPC error object (`{code, message, data}`) or
--- the result. Returns the request id (for `query.cancel`), or nil when the server can't start.
--- @param method string
--- @param params? table
--- @param cb fun(err: table?, result: any)
--- @param fast? boolean call `cb` straight from the I/O callback (no API calls allowed in it)
function M:request(method, params, cb, fast)
  local proc, err = self:start()
  local req = { cb = cb, fast = fast }
  if not proc then
    self:_answer(req, { code = M.EXITED, message = err })
    return nil
  end
  self.next_id = self.next_id + 1
  local id = self.next_id
  self.pending[id] = req
  local line = vim.json.encode({ jsonrpc = "2.0", id = id, method = method, params = params or vim.empty_dict() })
  local sent, write_err = pcall(proc.write, proc, line .. "\n")
  if not sent then
    self.pending[id] = nil
    self:_answer(req, { code = M.EXITED, message = tostring(write_err) })
    return nil
  end
  return id
end

--- Waits for the answer (up to `timeout` ms, default 5000). Returns `err, result`.
function M:request_sync(method, params, timeout)
  local done, err, result = false, nil, nil
  local id = self:request(method, params, function(e, r) done, err, result = true, e, r end, true)
  if not vim.wait(timeout or 5000, function() return done end, 10) then
    if id then
      self.pending[id] = nil
    end
    return { code = M.EXITED, message = method .. " timed out" }
  end
  return err, result
end

--- Sends a notification (no answer).
function M:notify(method, params)
  local proc = self:start()
  if proc then
    pcall(proc.write, proc, vim.json.encode({ jsonrpc = "2.0", method = method, params = params or vim.empty_dict() }) .. "\n")
  end
end

--- Closes stdin: the server answers what's in flight, then exits.
function M:stop()
  if self.proc then
    pcall(self.proc.write, self.proc, nil)
  end
end

function M:running()
  return self.proc ~= nil
end

return M
