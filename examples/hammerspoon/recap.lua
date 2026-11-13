-- recap.lua
-- A global hotkey that opens the triage web UI of recap, and the
-- recapNotify() function that the triage agent calls through `hs -c`.
--
-- Install: copy this file to ~/.hammerspoon/, then add this line to init.lua:
--   require("recap").bind({ "cmd", "alt" }, "q")
--
-- Options (the third argument of bind):
--   url  The URL of the recap server. The default is "http://127.0.0.1:7717/".

-- The triage agent runs `hs -c`, which needs the IPC module.
require("hs.ipc")

local M = {}

local opts = {}

-- Open the triage web UI in the default browser.
function M.openTriage()
    hs.urlevent.openURL(opts.url or "http://127.0.0.1:7717/")
end

-- Show a notification. A click on the notification opens the triage web UI.
function recapNotify(title, text)
    hs.notify.new(function() M.openTriage() end, {
        title = title,
        informativeText = text,
        withdrawAfter = 0,
    }):send()
end

-- require("recap").bind({ "cmd", "alt" }, "q")
function M.bind(mods, key, options)
    opts = options or {}
    return hs.hotkey.bind(mods, key, M.openTriage)
end

return M
