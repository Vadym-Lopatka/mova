-- Drive Conjure in headless nvim. env: CONJURE_DIR, NREPL_PORT, WORK_DIR
vim.opt.rtp:prepend(vim.env.CONJURE_DIR)
vim.cmd("runtime plugin/conjure.lua")
local port, dir = vim.env.NREPL_PORT, vim.env.WORK_DIR
local C = "conjure"
local function res(step, status, detail)
  io.stdout:write(("RESULT %s %s %s %s\n"):format(C, step, status, (detail or ""):gsub("\n", "|")))
end
local function logbuf()
  for _, b in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_get_name(b):match("conjure%-log") then return b end
  end
end
local function logtext()
  local b = logbuf(); if not b then return "" end
  return table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), "\n")
end
local mark = 0
local function since() local t = logtext(); local s = t:sub(mark + 1); mark = #t; return s end
local function wait_for(needle, ms)
  local ok = vim.wait(ms or 5000, function() return logtext():sub(mark + 1):find(needle, 1, true) ~= nil end, 100)
  return since(), ok
end
local function expect(step, needle, ms)
  local txt, ok = wait_for(needle, ms)
  if ok then res(step, "PASS") else res(step, "FAIL", "expected [" .. needle .. "] got [" .. txt .. "]") end
  return txt
end
local function ev(code) vim.cmd("ConjureEval " .. code) end

vim.cmd("cd " .. dir)
vim.cmd("edit " .. dir .. "/scratch.clj")
vim.bo.filetype = "clojure"
vim.wait(500)
vim.cmd("ConjureConnect 127.0.0.1:" .. port)
local _, ok = wait_for("(connected)", 8000)
if not ok then res("connect", "FAIL", logtext()) os.exit(1) end
res("connect", "PASS")
vim.wait(1500)  -- let Conjure finish its own startup evals (ns require etc.)
local startup = since()
io.stdout:write("INFO startup-log: " .. startup:gsub("\n", "|") .. "\n")

ev("(+ 1 2)");                          expect("eval", "3")
ev('(println "hello-out")');            expect("print", "hello-out")
ev("(/ 1 0)");                          local e = expect("error", "Divide by zero")
io.stdout:write("INFO error-display: " .. e:gsub("\n", "|") .. "\n")
ev("(def a 1) (def b 2) (+ a b)");      expect("multi", "3")

-- load file
local f = dir .. "/load.clj"
local fh = io.open(f, "w"); fh:write("(ns user)\n(defn sq [x]\n  (* x x))\n(println \"loaded\")\n"); fh:close()
vim.cmd("edit " .. f); vim.bo.filetype = "clojure"
since()
require("conjure.eval").file()
expect("load", "loaded")
vim.wait(500); since()
ev("(sq 9)");                           expect("load", "81") -- second check of the call (reported as its own line)

-- complete
local okc, comps = pcall(function() return require("conjure.eval")["completions-sync"]("ma") end)
if okc and type(comps) == "table" then
  local found = false
  for _, c in ipairs(comps) do if c.word == "map" then found = true end end
  io.stdout:write(("INFO complete count=%d\n"):format(#comps))
  if found then res("complete", "PASS") else res("complete", "FAIL", vim.inspect(comps):sub(1, 300)) end
else res("complete", "FAIL", tostring(comps)) end
since()

-- doc
require("conjure.client.clojure.nrepl.action")["doc-str"]({code = "map", origin = "x"})
local d = wait_for("Returns a lazy", 5000)
if d:find("Returns a lazy", 1, true) then res("doc", "PASS") else res("doc", "FAIL", d) end

-- interrupt
ev("(Thread/sleep 60000)")
vim.wait(1000); since()
require("conjure.client.clojure.nrepl.action").interrupt()
local it, iok = wait_for("Interrupted", 5000)
vim.wait(500)
since()
ev("(+ 20 22)")
local after, aok = wait_for("42", 5000)
if iok and aok then res("interrupt", "PASS") else res("interrupt", "FAIL", "interrupt-log=[" .. it .. "] next=[" .. after .. "]") end

-- stdin: Conjure asks with vim.fn.input; stub it
vim.fn.input = function() return "typed-text" end
since()
ev("(read-line)")
local s, sok = wait_for("typed-text", 6000)
if sok then res("stdin", "PASS") else res("stdin", "FAIL", s) end

-- close
vim.cmd("ConjureClientState")
pcall(function() require("conjure.client.clojure.nrepl.server").disconnect() end)
local c, cok = wait_for("(disconnected)", 3000)
if cok then res("close", "PASS") else res("close", "FAIL", c) end
io.stdout:write("FULLLOG:\n" .. logtext() .. "\n")
vim.cmd("qa!")
