defmodule Emerge.Engine.EventRegistry do
  @moduledoc false

  alias Emerge.Engine.Element
  alias Emerge.Engine.NodeId
  alias Emerge.Engine.Tree.Nearby
  alias Emerge.Engine.VNode

  @type events :: %{term() => {pid(), term()}}
  @type t :: %{binary() => events()}

  # One Raven patch: the registry holds exactly the nodes that have events.
  # Building one, or adding a subtree of freshly allocated ids
  # (`merge_new_tree/2`), only adds those nodes; `merge_tree/2` also deletes
  # the entry of every node without events, which a new id never has.
  @spec build(Element.t() | VNode.t() | nil) :: t()
  def build(tree), do: merge_new_tree(%{}, tree)

  @doc false
  @spec merge_new_tree(t(), Element.t() | VNode.t() | nil) :: t()
  def merge_new_tree(registry, nil), do: registry

  def merge_new_tree(registry, %VNode{} = vnode) do
    registry
    |> add_events(vnode.id, vnode.events)
    |> merge_new_children(vnode.children)
    |> merge_new_nearby(vnode.nearby)
  end

  def merge_new_tree(registry, %Element{} = element) do
    registry
    |> add_events(element.id, node_events(element.attrs))
    |> merge_new_children(element.children)
    |> merge_new_nearby(Nearby.nearby_children(element))
  end

  defp merge_new_children(registry, []), do: registry

  defp merge_new_children(registry, [child | rest]),
    do: registry |> merge_new_tree(child) |> merge_new_children(rest)

  defp merge_new_nearby(registry, []), do: registry

  defp merge_new_nearby(registry, [{_slot, child} | rest]),
    do: registry |> merge_new_tree(child) |> merge_new_nearby(rest)

  defp add_events(registry, id, events) when is_nil(id) or map_size(events) == 0, do: registry
  defp add_events(registry, id, events), do: put_events(registry, id, events)

  @spec merge_tree(t(), Element.t() | VNode.t() | nil) :: t()
  def merge_tree(registry, nil), do: registry

  def merge_tree(registry, %VNode{} = vnode) do
    registry
    |> put_events(vnode.id, vnode.events)
    |> then(fn registry ->
      registry = Enum.reduce(vnode.children, registry, &merge_tree(&2, &1))

      Enum.reduce(vnode.nearby, registry, fn {_slot, child}, next_registry ->
        merge_tree(next_registry, child)
      end)
    end)
  end

  def merge_tree(registry, %Element{} = element) do
    registry
    |> put_events(element.id, node_events(element.attrs))
    |> then(fn registry ->
      registry = Enum.reduce(element.children, registry, &merge_tree(&2, &1))

      Enum.reduce(Nearby.nearby_children(element), registry, fn {_slot, child}, next_registry ->
        merge_tree(next_registry, child)
      end)
    end)
  end

  # One Raven patch: a vnode without events has no entry to delete.
  @spec delete_vnode_subtree(t(), VNode.t()) :: t()
  def delete_vnode_subtree(registry, %VNode{} = vnode) do
    registry =
      if vnode.events == %{}, do: registry, else: delete_node(registry, vnode.id)

    registry
    |> delete_children(vnode.children)
    |> delete_nearby(vnode.nearby)
  end

  defp delete_children(registry, []), do: registry

  defp delete_children(registry, [child | rest]),
    do: registry |> delete_vnode_subtree(child) |> delete_children(rest)

  defp delete_nearby(registry, []), do: registry

  defp delete_nearby(registry, [{_slot, child} | rest]),
    do: registry |> delete_vnode_subtree(child) |> delete_nearby(rest)

  @event_attrs [
    on_click: :click,
    on_press: :press,
    on_swipe_up: :swipe_up,
    on_swipe_down: :swipe_down,
    on_swipe_left: :swipe_left,
    on_swipe_right: :swipe_right,
    on_mouse_down: :mouse_down,
    on_mouse_up: :mouse_up,
    on_mouse_enter: :mouse_enter,
    on_mouse_leave: :mouse_leave,
    on_mouse_move: :mouse_move,
    on_change: :change,
    on_scroll: :scroll,
    on_focus: :focus,
    on_blur: :blur
  ]

  @key_event_attrs [on_key_down: :key_down, on_key_up: :key_up, on_key_press: :key_press]

  # One Raven patch: one pass over the attrs a node has, dispatching each key
  # by function clause, instead of looking up all 19 event attrs on every
  # node. Each attr writes its own registry keys, so the order doesn't matter.
  @spec node_events(map()) :: events()
  def node_events(attrs) when is_map(attrs), do: collect_events(Map.keys(attrs), attrs, %{})

  defp collect_events([], _attrs, events), do: events

  for {attr, event} <- @event_attrs do
    defp collect_events([unquote(attr) | keys], attrs, events) do
      events = register_event(events, :erlang.map_get(unquote(attr), attrs), unquote(event))
      collect_events(keys, attrs, events)
    end
  end

  for {attr, event_type} <- @key_event_attrs do
    defp collect_events([unquote(attr) | keys], attrs, events) do
      value = :erlang.map_get(unquote(attr), attrs)
      collect_events(keys, attrs, register_key_events(events, value, unquote(event_type)))
    end
  end

  defp collect_events([:virtual_key | keys], attrs, events) do
    value = :erlang.map_get(:virtual_key, attrs)
    collect_events(keys, attrs, register_virtual_key_hold_event(events, value))
  end

  defp collect_events([_key | keys], attrs, events), do: collect_events(keys, attrs, events)

  @spec put_events(t(), non_neg_integer() | nil, events()) :: t()
  def put_events(registry, nil, _events), do: registry
  def put_events(registry, id, events) when map_size(events) == 0, do: delete_node(registry, id)

  def put_events(registry, id, events) when is_integer(id) do
    Map.put(registry, NodeId.encode(id), events)
  end

  @spec delete_node(t(), non_neg_integer() | nil) :: t()
  def delete_node(registry, nil), do: registry
  def delete_node(registry, id) when is_integer(id), do: Map.delete(registry, NodeId.encode(id))

  # Compatibility wrapper for callers that only know about attrs.
  @spec put_node(t(), non_neg_integer() | nil, map()) :: t()
  def put_node(registry, id, attrs), do: put_events(registry, id, node_events(attrs))

  defp register_event(events, {pid, msg}, event) when is_pid(pid),
    do: Map.put(events, event, {pid, msg})

  defp register_event(events, _value, _event), do: events

  defp register_key_events(events, bindings, event_type) when is_list(bindings) do
    Enum.reduce(bindings, events, fn binding, next_events ->
      register_key_event(next_events, event_type, binding)
    end)
  end

  defp register_key_events(events, _value, _event_type), do: events

  defp register_key_event(events, event_type, %{route: route, payload: {pid, msg}})
       when is_binary(route) and is_pid(pid) do
    Map.put(events, {event_type, route}, {pid, msg})
  end

  defp register_key_event(events, _event_type, _binding), do: events

  defp register_virtual_key_hold_event(events, %{hold: {:event, {pid, msg}}}) when is_pid(pid),
    do: Map.put(events, :virtual_key_hold, {pid, msg})

  defp register_virtual_key_hold_event(events, _value), do: events
end
