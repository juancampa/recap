-- recap.lua
-- A global hotkey that brings the recap queue window to the front, and the
-- recapNotify() function that the triage agent calls through `hs -c`.
--
-- Install: copy this file to ~/.hammerspoon/, then add this line to init.lua:
--   require("recap").bind({ "cmd", "alt" }, "q", { recap = "/path/to/recap" })
--
-- Options:
--   recap  The recap binary. The default is "recap" on the login PATH.
--   args   More arguments for the TUI, for example "--server http://127.0.0.1:7718".
--
-- The TUI sets the terminal title "recap queue" when it starts with --queue.
-- The hotkey focuses the Ghostty window that has this title. If no such
-- window exists, the hotkey opens one in the running Ghostty instance through
-- AppleScript. `open -na Ghostty` starts a second Ghostty process, so this
-- module does not use it.

-- The triage agent runs `hs -c`, which needs the IPC module.
require("hs.ipc")

local M = {}

local TITLE = "recap queue"

local opts = {}
-- The last queue window that the module found. A lookup through the cache
-- is faster than a scan of all the Ghostty windows.
local cached = nil

local function findWindow()
    if cached and cached:application() and cached:title() == TITLE then
        return cached
    end
    cached = nil
    local app = hs.application.get("Ghostty")
    if not app then
        return nil
    end
    for _, w in ipairs(app:allWindows()) do
        if w:title() == TITLE then
            cached = w
            return w
        end
    end
    return nil
end

-- The login shell sets the full PATH. A window that Ghostty starts from a
-- GUI process gets only the short PATH of launchd.
local function buildScript()
    local command = (opts.recap or "recap") .. " --queue"
    if opts.args then
        command = command .. " " .. opts.args
    end
    return string.format([[
tell application "Ghostty"
  set cfg to new surface configuration
  set command of cfg to "/bin/zsh -lc 'exec %s'"
  new window with configuration cfg
end tell
]], command)
end

local function launch()
    local ok, _, err = hs.osascript.applescript(buildScript())
    if not ok then
        hs.printf("recap: osascript failed: %s", tostring(err))
        return
    end
    -- The TUI sets the title after it connects to the server, so wait for
    -- the title. The wait stops after 30 s.
    local attempts = 0
    local function tryFocus()
        local w = findWindow()
        if w then
            w:focus()
            return
        end
        attempts = attempts + 1
        if attempts < 300 then
            hs.timer.doAfter(0.1, tryFocus)
        end
    end
    tryFocus()
end

function M.focusQueue()
    local w = findWindow()
    if w then
        w:focus()
    else
        launch()
    end
end

-- Show a notification. A click on the notification focuses the queue window.
function recapNotify(title, text)
    hs.notify.new(function() M.focusQueue() end, {
        title = title,
        informativeText = text,
        withdrawAfter = 0,
    }):send()
end

-- require("recap").bind({ "cmd", "alt" }, "q", { recap = "/path/to/recap" })
function M.bind(mods, key, options)
    opts = options or {}
    return hs.hotkey.bind(mods, key, M.focusQueue)
end

return M
