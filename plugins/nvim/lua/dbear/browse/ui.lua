--- Small UI pieces of the browser: the SQL confirmation float, the help float and a yes/no
--- question. Tests replace `M.ask`.

local M = {}

local ns = vim.api.nvim_create_namespace("dbear.browse.ui")

local function float(lines, opts)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.bo[buf].bufhidden = "wipe"
  local width = 20
  for _, l in ipairs(lines) do
    width = math.max(width, vim.api.nvim_strwidth(l))
  end
  width = math.min(width + 2, vim.o.columns - 6)
  local rows = 0
  for _, l in ipairs(lines) do
    rows = rows + (opts.wrap and math.max(1, math.ceil(vim.api.nvim_strwidth(l) / width)) or 1)
  end
  local height = math.min(rows, math.max(3, vim.o.lines - 8))
  local win = vim.api.nvim_open_win(buf, true, {
    relative = "editor",
    row = math.floor((vim.o.lines - height) / 2) - 1,
    col = math.floor((vim.o.columns - width) / 2),
    width = width,
    height = height,
    style = "minimal",
    border = "rounded",
    title = opts.title,
    title_pos = opts.title and "center" or nil,
    footer = opts.footer,
    footer_pos = opts.footer and "center" or nil,
  })
  vim.wo[win].wrap = opts.wrap or false
  vim.wo[win].cursorline = opts.cursorline or false
  return buf, win
end

local verbs = { DELETE = "DbearDelete", UPDATE = "DbearUpdate", INSERT = "DbearInsert" }

--- Shows the statements; `done(true)` on y/<CR>, `done(false)` on n/q/<Esc> or leaving the window.
--- @param summary string e.g. "1 update, 1 delete"
--- @param statements { sql: string }[]
function M.confirm(summary, statements, done)
  local lines, marks = { summary, "" }, {}
  for _, s in ipairs(statements) do
    local verb = s.sql:match("^(%u+)")
    marks[#marks + 1] = { #lines, verb and verbs[verb] }
    vim.list_extend(lines, vim.split(s.sql, "\n", { plain = true }))
  end
  -- Wrapped: every statement is seen whole.
  local buf, win = float(lines, { title = " Apply these changes? ", footer = " y/<CR> apply · n/q/<Esc> cancel ", wrap = true })
  vim.api.nvim_buf_set_extmark(buf, ns, 0, 0, { end_col = #lines[1], hl_group = "DbearSummary" })
  for _, m in ipairs(marks) do
    if m[2] then
      vim.api.nvim_buf_set_extmark(buf, ns, m[1], 0, { end_col = 6, hl_group = m[2] })
    end
  end
  vim.b[buf].dbear_confirm = true
  local answered = false
  local function answer(yes)
    if answered then
      return
    end
    answered = true
    if vim.api.nvim_win_is_valid(win) then
      vim.api.nvim_win_close(win, true)
    end
    done(yes)
  end
  for _, k in ipairs({ "y", "<CR>" }) do
    vim.keymap.set("n", k, function() answer(true) end, { buffer = buf, nowait = true })
  end
  for _, k in ipairs({ "n", "q", "<Esc>" }) do
    vim.keymap.set("n", k, function() answer(false) end, { buffer = buf, nowait = true })
  end
  vim.api.nvim_create_autocmd("WinLeave", { buffer = buf, once = true, callback = function() vim.schedule(function() answer(false) end) end })
  return win
end

--- A float with `lines`; q/<Esc> close it.
function M.info(title, lines)
  local buf, win = float(lines, { title = " " .. title .. " ", wrap = true })
  for _, k in ipairs({ "q", "<Esc>" }) do
    vim.keymap.set("n", k, function() pcall(vim.api.nvim_win_close, win, true) end, { buffer = buf, nowait = true })
  end
  vim.api.nvim_create_autocmd("WinLeave", { buffer = buf, once = true, callback = function() pcall(vim.api.nvim_win_close, win, true) end })
  return win
end

--- A yes/no question (default no).
function M.ask(question)
  return vim.fn.confirm(question, "&Yes\n&No", 2) == 1
end

return M
