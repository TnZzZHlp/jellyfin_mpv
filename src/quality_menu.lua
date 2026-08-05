local entries = {
    { id = "adaptive", label = "自动调整码率" },
    { id = "auto", label = "自动（不限制）" },
    { id = "bitrate-40000000", label = "码率上限：40 Mbps" },
    { id = "bitrate-20000000", label = "码率上限：20 Mbps" },
    { id = "bitrate-12000000", label = "码率上限：12 Mbps" },
    { id = "bitrate-8000000", label = "码率上限：8 Mbps" },
    { id = "bitrate-6000000", label = "码率上限：6 Mbps" },
    { id = "bitrate-4000000", label = "码率上限：4 Mbps" },
    { id = "bitrate-3000000", label = "码率上限：3 Mbps" },
    { id = "bitrate-1500000", label = "码率上限：1.5 Mbps" },
}

local visible = false
local selected = 1
local current_id = "auto"
local current_label = "自动（不限制）"

local function broadcast(name, ...)
    mp.commandv("script-message", name, ...)
end

local function selected_entry()
    return entries[selected]
end

local function select_current_entry()
    for index, entry in ipairs(entries) do
        if entry.id == current_id then
            return index
        end
    end
    return 1
end

local function render()
    if not visible then
        return
    end

    local lines = {
        "Jellyfin MPV 码率",
        "当前：" .. current_label,
        "",
    }
    for index, entry in ipairs(entries) do
        local marker = index == selected and "> " or "  "
        table.insert(lines, marker .. entry.label)
    end
    table.insert(lines, "")
    table.insert(lines, "上/下方向键选择  Enter 应用  Esc/F9 关闭")
    mp.osd_message(table.concat(lines, "\n"), 60)
end

local function remove_menu_bindings()
    mp.remove_key_binding("jellyfin-mpv-bitrate-up")
    mp.remove_key_binding("jellyfin-mpv-bitrate-down")
    mp.remove_key_binding("jellyfin-mpv-bitrate-apply")
    mp.remove_key_binding("jellyfin-mpv-bitrate-close")
end

local function close_menu()
    visible = false
    remove_menu_bindings()
    mp.osd_message("", 0)
end

local function move_selection(delta)
    selected = ((selected - 1 + delta) % #entries) + 1
    render()
end

local function apply_selection()
    local entry = selected_entry()
    current_id = entry.id
    current_label = entry.label
    close_menu()
    mp.osd_message("Jellyfin MPV\n正在切换：" .. entry.label, 3)
    broadcast("jellyfin-mpv-bitrate", entry.id)
end

local function open_menu()
    visible = true
    selected = select_current_entry()
    mp.add_forced_key_binding("UP", "jellyfin-mpv-bitrate-up", function()
        move_selection(-1)
    end, { repeatable = true })
    mp.add_forced_key_binding("DOWN", "jellyfin-mpv-bitrate-down", function()
        move_selection(1)
    end, { repeatable = true })
    mp.add_forced_key_binding("ENTER", "jellyfin-mpv-bitrate-apply", apply_selection)
    mp.add_forced_key_binding("ESC", "jellyfin-mpv-bitrate-close", close_menu)
    render()
end

local function toggle_menu()
    if visible then
        close_menu()
    else
        open_menu()
    end
end

mp.add_forced_key_binding("F9", "jellyfin-mpv-bitrate-menu", toggle_menu)

mp.register_script_message("jellyfin-mpv-bitrate-selected", function(id, label)
    current_id = id or "auto"
    current_label = label or "自动（不限制）"
    if visible then
        selected = select_current_entry()
        render()
    end
end)
