defmodule Emerge.Engine.Tree.Attrs do
  @moduledoc false

  @runtime_attrs [
    :scroll_x,
    :scroll_y,
    :scroll_max,
    :scroll_max_x,
    :scroll_bounds,
    :scroll_clip_bounds,
    :clip_bounds,
    :clip_content,
    :text_baseline_offset,
    :__layer,
    :scroll_capture,
    :mouse_over_active,
    :mouse_down_active,
    :focused_active,
    :text_input_focused,
    :text_input_cursor,
    :text_input_selection_anchor,
    :nearby_behind,
    :nearby_in_front,
    :nearby_outside,
    :__attrs_hash
  ]

  @volatile_attrs [
    :scroll_x,
    :scroll_y,
    :scroll_max,
    :scroll_max_x,
    :scroll_bounds,
    :scroll_clip_bounds,
    :clip_bounds,
    :clip_content,
    :text_baseline_offset,
    :__layer,
    :scroll_capture,
    :mouse_over_active,
    :mouse_down_active,
    :focused_active,
    :text_input_focused,
    :text_input_cursor,
    :text_input_selection_anchor,
    :__attrs_hash
  ]

  @doc false
  def runtime_attrs, do: @runtime_attrs

  # One Raven patch: an element's attrs rarely hold a runtime attr, so check
  # its few keys before dropping all 21 (per node on every encode).
  @doc false
  def strip_runtime_attrs(attrs) when is_map(attrs) do
    if any_runtime_attr?(Map.keys(attrs)),
      do: Map.drop(attrs, @runtime_attrs),
      else: attrs
  end

  defp any_runtime_attr?([]), do: false
  defp any_runtime_attr?([key | keys]), do: runtime_attr?(key) or any_runtime_attr?(keys)

  for key <- @runtime_attrs do
    defp runtime_attr?(unquote(key)), do: true
  end

  defp runtime_attr?(_key), do: false

  @doc false
  def attrs_hash(attrs) when is_map(attrs) do
    attrs
    |> Map.drop(@volatile_attrs)
    |> :erlang.phash2()
  end
end
