local utils = require "mp.utils"

local thumbnail_width = 0
local thumbnail_height = 0
local enabled = false

local function broadcast(name, ...)
    mp.commandv("script-message", name, ...)
end

local function send_thumbfast_info()
    local json = utils.format_json({
        width = thumbnail_width,
        height = thumbnail_height,
        disabled = not enabled,
        available = enabled,
        overlay_id = 47,
    })
    if json then
        broadcast("thumbfast-info", json)
    end
end

local function request_preview(seconds, x, y, width, height)
    if not enabled or seconds == nil or x == nil or y == nil then
        return
    end
    broadcast(
        "jellyfin-mpv-preview",
        tostring(seconds),
        tostring(x),
        tostring(y),
        tostring(width or thumbnail_width),
        tostring(height or thumbnail_height)
    )
end

local function clear_preview()
    if enabled then
        broadcast("jellyfin-mpv-preview-clear")
    end
end

mp.register_script_message("jellyfin-mpv-thumbnail-info", function(width, height)
    thumbnail_width = tonumber(width) or 0
    thumbnail_height = tonumber(height) or 0
    enabled = thumbnail_width > 0 and thumbnail_height > 0
    send_thumbfast_info()
end)

-- uosc and other third-party OSC scripts use the thumbfast message protocol.
mp.register_script_message("thumb", function(seconds, x, y)
    request_preview(seconds, x, y, thumbnail_width, thumbnail_height)
end)
mp.register_script_message("clear", clear_preview)

-- Newer versions of mpv's built-in OSC expose preview requests through user-data.
mp.observe_property("user-data/osc/draw-preview", "native", function(_, preview)
    if preview then
        request_preview(
            preview["hover-sec"],
            preview.x,
            preview.y,
            preview.w,
            preview.h
        )
    else
        clear_preview()
    end
end)
