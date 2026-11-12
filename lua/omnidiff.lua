-- OmniDiff for Rex — AST-aware Git diff, hunk staging, Kitty image diffs, and AI code review.
-- Installed to ~/.config/rex/omnidiff.lua by `rex-omnidiff install`.

local BIN = "@REX_OMNIDIFF_BIN@"
local SHELL_FLAVOR = "com.superlogical.terminal.shell"
local FLOAT_LABEL = "omnidiff-float"
local DOCK_LABEL = "omnidiff-dock"

local function resolve_active_window(view)
  if not view or not view.windows then
    return nil
  end
  for _, w in ipairs(view.windows) do
    if w.window_id == view.active_window_id then
      return w
    end
  end
  return view.windows[1]
end

local function resolve_block_cwd(block_id)
  if not block_id or block_id == "" then
    return nil
  end
  local proc = rex.block.call("com.superlogical.terminal", "process", {
    block_id = block_id,
  })
  if proc then
    if proc.foreground and proc.foreground.cwd and proc.foreground.cwd ~= "" then
      return proc.foreground.cwd
    end
    if proc.ancestors then
      for _, anc in ipairs(proc.ancestors) do
        if anc.cwd and anc.cwd ~= "" then
          return anc.cwd
        end
      end
    end
    if proc.home and proc.home ~= "" then
      return proc.home
    end
  end
  local pwd_res = rex.block.call("com.superlogical.terminal", "pwd", {
    block_id = block_id,
  })
  if pwd_res and pwd_res.pwd and pwd_res.pwd ~= "" then
    local path = string.match(pwd_res.pwd, "^file://[^/]*(/.*)$")
    if path and path ~= "" then
      return path
    end
  end
  return nil
end

local function find_source_terminal_block(win, preferred_block_id)
  if not win or not win.layers then
    return preferred_block_id
  end
  local fallback = nil
  for _, layer in ipairs(win.layers) do
    for _, blk in ipairs(layer.blocks or {}) do
      local lbl = blk.label or ""
      if lbl ~= FLOAT_LABEL and lbl ~= DOCK_LABEL then
        if preferred_block_id and blk.block_id == preferred_block_id then
          return blk.block_id
        end
        if win.focused_block_id and blk.block_id == win.focused_block_id then
          fallback = blk.block_id
        elseif not fallback then
          fallback = blk.block_id
        end
      end
    end
  end
  return fallback or preferred_block_id
end

rex.action{
  name = "omnidiff_toggle",
  title = "OmniDiff: Toggle Floating Review Window",
  description = "Open or close the wide (94% x 90%) Side-by-Side OmniDiff review layer in the active window.",
  category = "Git & Review",
  keywords = "git diff omnidiff review stage hunk ast kitty",
  args = {
    cwd = "string?",
    scope = "string?",
    path = "string?",
  },
  run = function(ctx, args)
    local view, err = rex.session.view{}
    if not view then
      error(err or "failed to inspect Rex session", 0)
    end
    local win = resolve_active_window(view)
    if not win then
      error("no active window in current session", 0)
    end

    -- If a floating OmniDiff layer is already open in this window, close it (toggle off).
    for _, layer in ipairs(win.layers or {}) do
      if layer.kind ~= "tiled" then
        for _, blk in ipairs(layer.blocks or {}) do
          if blk.label == FLOAT_LABEL then
            local _, close_err = rex.session.close_layer{
              layer_id = layer.layer_id,
            }
            if close_err ~= nil then
              error(close_err, 0)
            end
            return { closed = true, layer_id = layer.layer_id }
          end
        end
      end
    end

    local src_block = find_source_terminal_block(win, ctx.block_id)
    local cwd = (args and args.cwd) or resolve_block_cwd(src_block)
    local cmd = { BIN, "ui", "--mode", "float" }
    if src_block and src_block ~= "" then
      table.insert(cmd, "--source-block")
      table.insert(cmd, src_block)
    end
    if cwd and cwd ~= "" then
      table.insert(cmd, "--cwd")
      table.insert(cmd, cwd)
    end
    if args and args.scope and args.scope ~= "" then
      table.insert(cmd, "--scope")
      table.insert(cmd, args.scope)
    end
    if args and args.path and args.path ~= "" then
      table.insert(cmd, "--path")
      table.insert(cmd, args.path)
    end

    local opts = {
      shell = "none",
      command = cmd,
    }
    if cwd and cwd ~= "" then
      opts.cwd = cwd
    end

    local created, create_err = rex.session.new_layer{
      window_id = win.window_id,
      bounds = { 0.03, 0.05, 0.94, 0.90 },
      focus = true,
      layout = {
        block = {
          flavor = SHELL_FLAVOR,
          label = FLOAT_LABEL,
          options = opts,
        },
      },
    }
    if create_err ~= nil then
      error(create_err, 0)
    end
    local new_block_id = created.block_ids and created.block_ids[1] or nil
    if new_block_id then
      rex.session.set_block_label{
        block_id = new_block_id,
        label = FLOAT_LABEL,
      }
    end
    return {
      opened = true,
      mode = "float",
      layer_id = created.layer_id,
      block_id = new_block_id,
      cwd = cwd,
    }
  end,
}

rex.action{
  name = "omnidiff_dock",
  title = "OmniDiff: Toggle Docked Changes Panel",
  description = "Open or close the docked right-hand OmniDiff split panel that follows your focused terminal's Git repository.",
  category = "Git & Review",
  keywords = "git diff omnidiff dock split panel changes review",
  args = {
    cwd = "string?",
    ratio = "number?",
  },
  run = function(ctx, args)
    local view, err = rex.session.view{}
    if not view then
      error(err or "failed to inspect Rex session", 0)
    end
    local win = resolve_active_window(view)
    if not win then
      error("no active window in current session", 0)
    end

    -- If a docked OmniDiff split block is already open in this window, close it (toggle off).
    for _, layer in ipairs(win.layers or {}) do
      if layer.kind == "tiled" then
        for _, blk in ipairs(layer.blocks or {}) do
          if blk.label == DOCK_LABEL then
            local _, close_err = rex.call("block.close", {
              block_id = blk.block_id,
            })
            if close_err ~= nil then
              error(close_err, 0)
            end
            return { closed = true, block_id = blk.block_id }
          end
        end
      end
    end

    local src_block = find_source_terminal_block(win, ctx.block_id)
    local cwd = (args and args.cwd) or resolve_block_cwd(src_block)
    local cmd = { BIN, "ui", "--mode", "dock" }
    if src_block and src_block ~= "" then
      table.insert(cmd, "--source-block")
      table.insert(cmd, src_block)
    end
    if cwd and cwd ~= "" then
      table.insert(cmd, "--cwd")
      table.insert(cmd, cwd)
    end

    local opts = {
      shell = "none",
      command = cmd,
    }
    if cwd and cwd ~= "" then
      opts.cwd = cwd
    end

    -- Anchor to the tiled layer's root split if present so the docked panel sits on the right edge of the window.
    local anchor_id = src_block
    for _, layer in ipairs(win.layers or {}) do
      if layer.kind == "tiled" then
        if layer.layout and layer.layout.split and layer.layout.split.split_id then
          anchor_id = layer.layout.split.split_id
        elseif not anchor_id and layer.blocks and layer.blocks[1] then
          anchor_id = layer.blocks[1].block_id
        end
        break
      end
    end

    local split_ratio = (args and args.ratio) or 0.64
    local created, split_err = rex.session.new_split{
      anchor_id = anchor_id,
      direction = "horizontal",
      side = "after",
      ratio = split_ratio,
      focus = true,
      layout = {
        block = {
          flavor = SHELL_FLAVOR,
          label = DOCK_LABEL,
          options = opts,
        },
      },
    }
    if split_err ~= nil then
      error(split_err, 0)
    end
    local new_block_id = created.block_ids and created.block_ids[1] or nil
    if new_block_id then
      rex.session.set_block_label{
        block_id = new_block_id,
        label = DOCK_LABEL,
      }
    end
    return {
      opened = true,
      mode = "dock",
      block_id = new_block_id,
      cwd = cwd,
    }
  end,
}

-- Bindings (skip if the user already bound these keys earlier in init.lua)
local existing_float = rex.bindings{ key = "shift+cmd+g" }
if not existing_float or #existing_float == 0 then
  rex.bind("shift+cmd+g", "omnidiff_toggle")
end

local existing_dock = rex.bindings{ key = "alt+cmd+g" }
if not existing_dock or #existing_dock == 0 then
  rex.bind("alt+cmd+g", "omnidiff_dock")
end
