defmodule Emerge.Engine.Reconcile do
  @moduledoc """
  Reconcile Emerge.Engine.Element trees into stable node ids and patch operations.
  """

  alias Emerge.Engine.Element
  alias Emerge.Engine.EventRegistry
  alias Emerge.Engine.Patch
  alias Emerge.Engine.ScrollTarget
  alias Emerge.Engine.Tree.Attrs, as: TreeAttrs
  alias Emerge.Engine.VNode

  @type ctx :: %{
          next_id: non_neg_integer(),
          seen: MapSet.t()
        }

  @type result :: {VNode.t(), [Patch.patch()], Element.t()}

  @doc """
  Assign fresh node ids to a tree without a previous version.
  """
  @spec assign_ids(Element.t()) :: {VNode.t(), Element.t()}
  def assign_ids(%Element{} = element) do
    {vnode, assigned, _next_node_id} = assign_ids(element, 1)
    {vnode, assigned}
  end

  @spec assign_ids(Element.t(), non_neg_integer()) :: {VNode.t(), Element.t(), non_neg_integer()}
  def assign_ids(%Element{} = element, next_id)
      when is_integer(next_id) and next_id > 0 do
    validate_viewport_root!(element)

    ctx = %{next_id: next_id, seen: MapSet.new()}
    {vnode, assigned, ctx} = build_fresh_subtree(element, ctx)
    {vnode, assigned, ctx.next_id}
  end

  @doc """
  Reconcile a new tree against the previous vdom.
  """
  @spec reconcile(VNode.t() | nil, Element.t()) :: result()
  def reconcile(old_vnode, %Element{} = element) do
    {vnode, patches, assigned, _next_node_id} = reconcile(old_vnode, element, 1)
    {vnode, patches, assigned}
  end

  @spec reconcile(VNode.t() | nil, Element.t(), non_neg_integer()) ::
          {VNode.t(), [Patch.patch()], Element.t(), non_neg_integer()}
  def reconcile(nil, %Element{} = element, next_id)
      when is_integer(next_id) and next_id > 0 do
    validate_viewport_root!(element)

    ctx = %{next_id: next_id, seen: MapSet.new()}
    {vnode, assigned, ctx} = build_fresh_subtree(element, ctx)
    {vnode, [], assigned, ctx.next_id}
  end

  def reconcile(%VNode{} = old_vnode, %Element{} = element, next_id)
      when is_integer(next_id) and next_id > 0 do
    validate_viewport_root!(element)

    ctx = %{next_id: next_id, seen: MapSet.new()}

    if reusable_root?(old_vnode, element) do
      {vnode, patches, assigned, ctx} = reconcile_matched_node(old_vnode, element, ctx)
      {vnode, patches, assigned, ctx.next_id}
    else
      {vnode, assigned, ctx} = build_fresh_subtree(element, ctx)

      {vnode, [{:remove, old_vnode.id}, {:insert_subtree, nil, 0, assigned}], assigned,
       ctx.next_id}
    end
  end

  @doc """
  Reconcile a new tree against the previous vdom without constructing a full
  assigned `%Element{}` tree.

  This is the runtime hot path: it returns the next vdom, patch list, and next
  id counter. Insert patches still carry assigned inserted subtrees because the
  patch wire format needs ids for new nodes.
  """
  @spec reconcile_patches(VNode.t() | nil, Element.t(), non_neg_integer()) ::
          {VNode.t(), [Patch.patch()], non_neg_integer()}
  def reconcile_patches(old_vnode, %Element{} = element, next_id)
      when is_integer(next_id) and next_id > 0 do
    {vnode, patches, _event_registry, next_id} =
      reconcile_patches_optional_event_registry(old_vnode, element, next_id, nil)

    {vnode, patches, next_id}
  end

  @doc false
  @spec reconcile_patches_and_event_registry(VNode.t() | nil, Element.t(), non_neg_integer()) ::
          {VNode.t(), [Patch.patch()], EventRegistry.t(), non_neg_integer()}
  def reconcile_patches_and_event_registry(old_vnode, %Element{} = element, next_id)
      when is_integer(next_id) and next_id > 0 do
    {vnode, patches, next_id} = reconcile_patches(old_vnode, element, next_id)
    {vnode, patches, EventRegistry.build(vnode), next_id}
  end

  @doc false
  @spec reconcile_patches_and_event_registry(
          VNode.t() | nil,
          Element.t(),
          non_neg_integer(),
          EventRegistry.t()
        ) :: {VNode.t(), [Patch.patch()], EventRegistry.t(), non_neg_integer()}
  def reconcile_patches_and_event_registry(
        old_vnode,
        %Element{} = element,
        next_id,
        event_registry
      )
      when is_integer(next_id) and next_id > 0 and is_map(event_registry) do
    reconcile_patches_optional_event_registry(old_vnode, element, next_id, event_registry)
  end

  defp reconcile_patches_optional_event_registry(old_vnode, element, next_id, event_registry) do
    validate_viewport_root!(element)

    ctx =
      %{
        next_id: next_id,
        seen: MapSet.new(),
        assign_tree?: false
      }
      |> maybe_init_event_registry(event_registry)

    case old_vnode do
      nil ->
        ctx = reset_event_registry(ctx)
        {vnode, _assigned, ctx} = build_fresh_subtree_for_patches(element, ctx)
        {vnode, [], event_registry(ctx), ctx.next_id}

      %VNode{} = old ->
        if reusable_root?(old, element) do
          {vnode, patches, _assigned, ctx} = reconcile_matched_node(old, element, ctx)
          {vnode, patches, event_registry(ctx), ctx.next_id}
        else
          ctx = reset_event_registry(ctx)
          {vnode, assigned, ctx} = build_fresh_subtree_for_patches(element, ctx)

          {vnode, [{:remove, old.id}, {:insert_subtree, nil, 0, assigned}], event_registry(ctx),
           ctx.next_id}
        end
    end
  end

  defp maybe_init_event_registry(ctx, nil), do: ctx

  defp maybe_init_event_registry(ctx, event_registry) when is_map(event_registry),
    do: Map.put(ctx, :event_registry, event_registry)

  defp reset_event_registry(%{event_registry: _registry} = ctx), do: %{ctx | event_registry: %{}}
  defp reset_event_registry(ctx), do: ctx

  defp event_registry(%{event_registry: registry}), do: registry
  defp event_registry(_ctx), do: %{}

  # One Raven patch: a subtree built by `Emerge.UI.lazy/3` whose tag (id and
  # deps) is the one it was built with last time is the same subtree, so it
  # is reused as it stands: no walk, no patches, no event registry changes.
  # Its keys still count against the tree's (uniqueness, scroll targets).
  # Only in the patch-only hot path, which builds no assigned tree; and never
  # for a subtree with a keyed scroll position, whose target id can depend on
  # nodes outside it.
  defp reconcile_matched_node(
         %VNode{memo: {tag, keys, true}} = old,
         %Element{memo: tag},
         %{assign_tree?: false} = ctx
       )
       when tag != nil do
    ctx =
      Enum.reduce(keys, ctx, fn {key, id}, ctx ->
        ctx |> ensure_unique_key!(key) |> remember_key(key, id)
      end)

    {old, [], nil, ctx}
  end

  defp reconcile_matched_node(%VNode{} = old, %Element{} = element, ctx) do
    key = element_key(element)
    ctx = ctx |> ensure_unique_key!(key) |> collect_scroll_keys(element.attrs)

    {child_vnodes, child_elements, child_patches, ctx} =
      reconcile_children(old.children, element.children, old.id, ctx)

    {nearby_vnodes, nearby_elements, nearby_patches, ctx} =
      reconcile_nearby(old.nearby, element.nearby, old.id, ctx)

    {new_attrs, ctx} = resolve_scroll_attrs(element.attrs, old.attrs, ctx)
    ctx = remember_key(ctx, key, old.id)

    # One Raven patch: compare the attrs once for both the events and the
    # attr patch, and skip the patch-list reversals when nothing changed.
    same_attrs? = old.attrs === new_attrs
    events = vnode_events(old, new_attrs, same_attrs?)

    parent_patches =
      []
      |> maybe_set_nearby_mounts(old, nearby_vnodes)
      |> maybe_set_children(old, child_vnodes)
      |> maybe_set_attrs(old, new_attrs, old.id, same_attrs?)

    patches = concat_patches(parent_patches, child_patches, nearby_patches)

    vnode = %VNode{
      id: old.id,
      kind: element.type,
      key: key,
      attrs: new_attrs,
      events: events,
      children: child_vnodes,
      nearby: nearby_vnodes
    }

    vnode = put_memo(vnode, element.memo)

    assigned =
      maybe_assign_element(ctx, element, old.id, new_attrs, child_elements, nearby_elements)

    {vnode, patches, assigned, maybe_update_event_node(ctx, old.id, old.events, events)}
  end

  # One Raven patch: a leaf (most nodes) has nothing to reconcile.
  defp reconcile_children([], [], _parent_node_id, ctx), do: {[], [], [], ctx}

  defp reconcile_children(old_children, new_children, parent_node_id, ctx) do
    case children_mode(new_children) do
      :keyed -> reconcile_children_keyed(old_children, new_children, parent_node_id, ctx)
      :unkeyed -> reconcile_children_unkeyed(old_children, new_children, parent_node_id, ctx)
    end
  end

  # One Raven patch: a keyed child only ever matches an old child of the same
  # parent, so look keys up among the old parent's children instead of in an
  # index of the whole old tree, rebuilt (one more full walk) on every
  # reconcile.
  defp reconcile_children_keyed(old_children, new_children, parent_node_id, ctx) do
    old_by_key = keyed_index(old_children)

    {vnodes_rev, elements_rev, patches_rev, used_old_ids, ctx} =
      do_reconcile_children_keyed(
        new_children,
        0,
        old_by_key,
        parent_node_id,
        ctx,
        [],
        [],
        [],
        %{}
      )

    {patches_rev, ctx} = prepend_removed_children(old_children, used_old_ids, patches_rev, ctx)

    {Enum.reverse(vnodes_rev), Enum.reverse(elements_rev), Enum.reverse(patches_rev), ctx}
  end

  defp do_reconcile_children_keyed(
         [],
         _index,
         _scope,
         _parent_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev,
         used_old_ids
       ) do
    {vnodes_rev, elements_rev, patches_rev, used_old_ids, ctx}
  end

  defp do_reconcile_children_keyed(
         [child | rest],
         index,
         old_by_key,
         parent_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev,
         used_old_ids
       ) do
    key = element_key(child)

    case Map.get(old_by_key, key) do
      %VNode{kind: kind} = old_child when kind == child.type ->
        {vnode, child_patches, assigned, ctx} = reconcile_matched_node(old_child, child, ctx)

        do_reconcile_children_keyed(
          rest,
          index + 1,
          old_by_key,
          parent_node_id,
          ctx,
          [vnode | vnodes_rev],
          prepend_assigned(ctx, assigned, elements_rev),
          prepend_many(patches_rev, child_patches),
          Map.put(used_old_ids, old_child.id, true)
        )

      _ ->
        {vnode, assigned, ctx} = build_fresh_subtree_for_patches(child, ctx)

        do_reconcile_children_keyed(
          rest,
          index + 1,
          old_by_key,
          parent_node_id,
          ctx,
          [vnode | vnodes_rev],
          prepend_assigned(ctx, assigned, elements_rev),
          [{:insert_subtree, parent_node_id, index, assigned} | patches_rev],
          used_old_ids
        )
    end
  end

  # One Raven patch: body recursion builds the vnode, element and patch lists
  # in order, instead of three accumulators reversed at the end.
  defp reconcile_children_unkeyed(old_children, new_children, parent_node_id, ctx) do
    do_reconcile_children_unkeyed(old_children, new_children, 0, parent_node_id, ctx)
  end

  defp do_reconcile_children_unkeyed([], [], _index, _parent_node_id, ctx),
    do: {[], [], [], ctx}

  defp do_reconcile_children_unkeyed(
         [%VNode{} = old_child | old_rest],
         [%Element{} = child | new_rest],
         index,
         parent_node_id,
         ctx
       ) do
    if old_child.kind == child.type and old_child.key == nil and not has_key?(child) do
      {vnode, child_patches, assigned, ctx} = reconcile_matched_node(old_child, child, ctx)

      {vnodes, elements, patches, ctx} =
        do_reconcile_children_unkeyed(old_rest, new_rest, index + 1, parent_node_id, ctx)

      {[vnode | vnodes], prepend_assigned(ctx, assigned, elements), child_patches ++ patches, ctx}
    else
      {vnode, assigned, ctx} = build_fresh_subtree_for_patches(child, ctx)
      ctx = maybe_delete_event_tree(ctx, old_child)

      {vnodes, elements, patches, ctx} =
        do_reconcile_children_unkeyed(old_rest, new_rest, index + 1, parent_node_id, ctx)

      {[vnode | vnodes], prepend_assigned(ctx, assigned, elements),
       [{:remove, old_child.id}, {:insert_subtree, parent_node_id, index, assigned} | patches],
       ctx}
    end
  end

  defp do_reconcile_children_unkeyed(
         [],
         [%Element{} = child | new_rest],
         index,
         parent_node_id,
         ctx
       ) do
    {vnode, assigned, ctx} = build_fresh_subtree_for_patches(child, ctx)

    {vnodes, elements, patches, ctx} =
      do_reconcile_children_unkeyed([], new_rest, index + 1, parent_node_id, ctx)

    {[vnode | vnodes], prepend_assigned(ctx, assigned, elements),
     [{:insert_subtree, parent_node_id, index, assigned} | patches], ctx}
  end

  defp do_reconcile_children_unkeyed(
         [%VNode{} = old_child | old_rest],
         [],
         index,
         parent_node_id,
         ctx
       ) do
    ctx = maybe_delete_event_tree(ctx, old_child)

    {vnodes, elements, patches, ctx} =
      do_reconcile_children_unkeyed(old_rest, [], index + 1, parent_node_id, ctx)

    {vnodes, elements, [{:remove, old_child.id} | patches], ctx}
  end

  defp reconcile_nearby([], [], _host_node_id, ctx), do: {[], [], [], ctx}

  defp reconcile_nearby(old_nearby, new_nearby, host_node_id, ctx) do
    case nearby_mode(new_nearby) do
      :keyed -> reconcile_nearby_keyed(old_nearby, new_nearby, host_node_id, ctx)
      :unkeyed -> reconcile_nearby_unkeyed(old_nearby, new_nearby, host_node_id, ctx)
    end
  end

  defp reconcile_nearby_keyed(old_nearby, new_nearby, host_node_id, ctx) do
    old_by_key = keyed_index(old_nearby)

    {vnodes_rev, elements_rev, patches_rev, used_old_ids, ctx} =
      do_reconcile_nearby_keyed(
        new_nearby,
        0,
        old_by_key,
        host_node_id,
        ctx,
        [],
        [],
        [],
        %{}
      )

    {patches_rev, ctx} = prepend_removed_nearby(old_nearby, used_old_ids, patches_rev, ctx)

    {Enum.reverse(vnodes_rev), Enum.reverse(elements_rev), Enum.reverse(patches_rev), ctx}
  end

  defp do_reconcile_nearby_keyed(
         [],
         _index,
         _scope,
         _host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev,
         used_old_ids
       ) do
    {vnodes_rev, elements_rev, patches_rev, used_old_ids, ctx}
  end

  defp do_reconcile_nearby_keyed(
         [{slot, element} | rest],
         index,
         old_by_key,
         host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev,
         used_old_ids
       ) do
    key = element_key(element)

    case Map.get(old_by_key, key) do
      %VNode{kind: kind} = old_vnode when kind == element.type ->
        {vnode, mount_patches, assigned, ctx} = reconcile_matched_node(old_vnode, element, ctx)

        do_reconcile_nearby_keyed(
          rest,
          index + 1,
          old_by_key,
          host_node_id,
          ctx,
          [{slot, vnode} | vnodes_rev],
          prepend_nearby_assigned(ctx, slot, assigned, elements_rev),
          prepend_many(patches_rev, mount_patches),
          Map.put(used_old_ids, old_vnode.id, true)
        )

      _ ->
        {vnode, assigned, ctx} = build_fresh_subtree_for_patches(element, ctx)

        do_reconcile_nearby_keyed(
          rest,
          index + 1,
          old_by_key,
          host_node_id,
          ctx,
          [{slot, vnode} | vnodes_rev],
          prepend_nearby_assigned(ctx, slot, assigned, elements_rev),
          [{:insert_nearby_subtree, host_node_id, index, slot, assigned} | patches_rev],
          used_old_ids
        )
    end
  end

  defp reconcile_nearby_unkeyed(old_nearby, new_nearby, host_node_id, ctx) do
    {vnodes_rev, elements_rev, patches_rev, ctx} =
      do_reconcile_nearby_unkeyed(old_nearby, new_nearby, 0, host_node_id, ctx, [], [], [])

    {Enum.reverse(vnodes_rev), Enum.reverse(elements_rev), Enum.reverse(patches_rev), ctx}
  end

  defp do_reconcile_nearby_unkeyed(
         [],
         [],
         _index,
         _host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev
       ) do
    {vnodes_rev, elements_rev, patches_rev, ctx}
  end

  defp do_reconcile_nearby_unkeyed(
         [{_old_slot, %VNode{} = old_vnode} | old_rest],
         [{slot, %Element{} = element} | new_rest],
         index,
         host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev
       ) do
    if old_vnode.kind == element.type and old_vnode.key == nil and not has_key?(element) do
      {vnode, mount_patches, assigned, ctx} = reconcile_matched_node(old_vnode, element, ctx)

      do_reconcile_nearby_unkeyed(
        old_rest,
        new_rest,
        index + 1,
        host_node_id,
        ctx,
        [{slot, vnode} | vnodes_rev],
        prepend_nearby_assigned(ctx, slot, assigned, elements_rev),
        prepend_many(patches_rev, mount_patches)
      )
    else
      {vnode, assigned, ctx} = build_fresh_subtree_for_patches(element, ctx)
      ctx = maybe_delete_event_tree(ctx, old_vnode)

      do_reconcile_nearby_unkeyed(
        old_rest,
        new_rest,
        index + 1,
        host_node_id,
        ctx,
        [{slot, vnode} | vnodes_rev],
        prepend_nearby_assigned(ctx, slot, assigned, elements_rev),
        [
          {:insert_nearby_subtree, host_node_id, index, slot, assigned},
          {:remove, old_vnode.id}
          | patches_rev
        ]
      )
    end
  end

  defp do_reconcile_nearby_unkeyed(
         [],
         [{slot, %Element{} = element} | new_rest],
         index,
         host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev
       ) do
    {vnode, assigned, ctx} = build_fresh_subtree_for_patches(element, ctx)

    do_reconcile_nearby_unkeyed(
      [],
      new_rest,
      index + 1,
      host_node_id,
      ctx,
      [{slot, vnode} | vnodes_rev],
      prepend_nearby_assigned(ctx, slot, assigned, elements_rev),
      [{:insert_nearby_subtree, host_node_id, index, slot, assigned} | patches_rev]
    )
  end

  defp do_reconcile_nearby_unkeyed(
         [{_old_slot, %VNode{} = old_vnode} | old_rest],
         [],
         index,
         host_node_id,
         ctx,
         vnodes_rev,
         elements_rev,
         patches_rev
       ) do
    ctx = maybe_delete_event_tree(ctx, old_vnode)

    do_reconcile_nearby_unkeyed(
      old_rest,
      [],
      index + 1,
      host_node_id,
      ctx,
      vnodes_rev,
      elements_rev,
      [{:remove, old_vnode.id} | patches_rev]
    )
  end

  defp maybe_assign_element(
         %{assign_tree?: false},
         _element,
         _id,
         _attrs,
         _child_elements,
         _nearby_elements
       ),
       do: nil

  defp maybe_assign_element(_ctx, element, id, attrs, child_elements, nearby_elements) do
    %{element | id: id, attrs: attrs, children: child_elements, nearby: nearby_elements}
  end

  defp prepend_assigned(%{assign_tree?: false}, _assigned, elements_rev), do: elements_rev
  defp prepend_assigned(_ctx, assigned, elements_rev), do: [assigned | elements_rev]

  defp prepend_nearby_assigned(%{assign_tree?: false}, _slot, _assigned, elements_rev),
    do: elements_rev

  defp prepend_nearby_assigned(_ctx, slot, assigned, elements_rev),
    do: [{slot, assigned} | elements_rev]

  defp build_fresh_subtree_for_patches(%Element{} = element, ctx) do
    {vnode, assigned, ctx} = build_fresh_subtree(element, ctx)
    {vnode, assigned, maybe_register_event_tree(ctx, vnode)}
  end

  # One Raven patch: a fresh subtree's ids are new, so only its nodes with
  # events need adding.
  defp maybe_register_event_tree(%{event_registry: registry} = ctx, %VNode{} = vnode) do
    %{ctx | event_registry: EventRegistry.merge_new_tree(registry, vnode)}
  end

  defp maybe_register_event_tree(ctx, _vnode), do: ctx

  defp maybe_update_event_node(%{event_registry: _registry} = ctx, _id, events, events), do: ctx

  defp maybe_update_event_node(%{event_registry: registry} = ctx, id, _old_events, events) do
    %{ctx | event_registry: EventRegistry.put_events(registry, id, events)}
  end

  defp maybe_update_event_node(ctx, _id, _old_events, _events), do: ctx

  defp maybe_delete_event_tree(%{event_registry: registry} = ctx, %VNode{} = vnode) do
    %{ctx | event_registry: EventRegistry.delete_vnode_subtree(registry, vnode)}
  end

  defp maybe_delete_event_tree(ctx, _vnode), do: ctx

  defp vnode_events(%VNode{events: events}, _attrs, true) when is_map(events), do: events
  defp vnode_events(_old, attrs, _same_attrs?), do: EventRegistry.node_events(attrs)

  defp concat_patches([], [], []), do: []
  # The order the old prepend-and-reverse produced: nearby, children, parent.
  defp concat_patches(parent, children, nearby), do: nearby ++ children ++ parent

  # One Raven patch: children and nearby mounts are built by plain recursion
  # (no closures or accumulator tuples per child), and the element is copied
  # once, with its id, attrs and assigned children together.
  defp build_fresh_subtree(%Element{} = element, ctx) do
    key = element_key(element)
    ctx = ctx |> ensure_unique_key!(key) |> collect_scroll_keys(element.attrs)
    _ = children_mode(element.children)
    _ = nearby_mode(element.nearby)

    {id, ctx} = alloc_id(ctx)
    {child_vnodes, child_elements, ctx} = build_fresh_children(element.children, ctx)
    {nearby_vnodes, nearby_elements, ctx} = build_fresh_nearby(element.nearby, ctx)

    {attrs, ctx} = resolve_scroll_attrs(element.attrs, %{}, ctx)
    ctx = remember_key(ctx, key, id)

    vnode = %VNode{
      id: id,
      kind: element.type,
      key: key,
      attrs: attrs,
      events: EventRegistry.node_events(attrs),
      children: child_vnodes,
      nearby: nearby_vnodes
    }

    vnode = put_memo(vnode, element.memo)

    assigned = %{
      element
      | id: id,
        attrs: attrs,
        children: child_elements,
        nearby: nearby_elements
    }

    {vnode, assigned, ctx}
  end

  defp build_fresh_children([], ctx), do: {[], [], ctx}

  defp build_fresh_children([child | rest], ctx) do
    {vnode, assigned, ctx} = build_fresh_subtree(child, ctx)
    {vnodes, elements, ctx} = build_fresh_children(rest, ctx)
    {[vnode | vnodes], [assigned | elements], ctx}
  end

  defp build_fresh_nearby([], ctx), do: {[], [], ctx}

  defp build_fresh_nearby([{slot, child} | rest], ctx) do
    {vnode, assigned, ctx} = build_fresh_subtree(child, ctx)
    {vnodes, elements, ctx} = build_fresh_nearby(rest, ctx)
    {[{slot, vnode} | vnodes], [{slot, assigned} | elements], ctx}
  end

  # One Raven patch: a lazy subtree's vnode remembers its tag, every explicit
  # key in it with its node id, and whether it can be reused (no keyed scroll
  # position inside). A nested lazy subtree contributes its stored keys.
  defp put_memo(vnode, nil), do: vnode

  defp put_memo(%VNode{} = vnode, tag) do
    {keys, reusable?} = memo_walk(vnode, {[], true})
    %{vnode | memo: {tag, keys, reusable?}}
  end

  defp memo_walk(%VNode{} = vnode, {keys, reusable?}) do
    keys = if is_nil(vnode.key), do: keys, else: [{vnode.key, vnode.id} | keys]
    reusable? = reusable? and not keyed_scroll?(vnode.attrs)
    acc = Enum.reduce(vnode.children, {keys, reusable?}, &memo_walk_child/2)
    Enum.reduce(vnode.nearby, acc, fn {_slot, child}, acc -> memo_walk_child(child, acc) end)
  end

  defp memo_walk_child(%VNode{memo: {_tag, inner, inner_reusable?}}, {keys, reusable?}),
    do: {inner ++ keys, reusable? and inner_reusable?}

  defp memo_walk_child(%VNode{} = vnode, acc), do: memo_walk(vnode, acc)

  defp keyed_scroll?(attrs)
       when not is_map_key(attrs, :scroll_position_x) and
              not is_map_key(attrs, :scroll_position_y),
       do: false

  defp keyed_scroll?(attrs),
    do:
      match?(%ScrollTarget{}, Map.get(attrs, :scroll_position_x)) or
        match?(%ScrollTarget{}, Map.get(attrs, :scroll_position_y))

  # One Raven patch: skip the per-axis closures for the common element with no
  # scroll position (here and in resolve_scroll_attrs/3).
  defp collect_scroll_keys(ctx, attrs)
       when not is_map_key(attrs, :scroll_position_x) and
              not is_map_key(attrs, :scroll_position_y),
       do: ctx

  defp collect_scroll_keys(ctx, attrs) do
    if Enum.any?([:scroll_position_x, :scroll_position_y], fn axis ->
         Map.has_key?(attrs, axis) and not is_float(Map.fetch!(attrs, axis))
       end), do: Map.put(ctx, :collect_scroll_keys?, true), else: ctx
  end

  defp remember_key(ctx, nil, _id), do: ctx

  defp remember_key(%{collect_scroll_keys?: true} = ctx, key, id),
    do: Map.update(ctx, :new_key_ids, %{key => id}, &Map.put(&1, key, id))

  defp remember_key(ctx, _key, _id), do: ctx

  defp resolve_scroll_attrs(attrs, _old, ctx)
       when not is_map_key(attrs, :scroll_position_x) and
              not is_map_key(attrs, :scroll_position_y),
       do: {attrs, ctx}

  defp resolve_scroll_attrs(attrs, old, ctx) do
    Enum.reduce([:scroll_position_x, :scroll_position_y], {attrs, ctx}, fn axis, {attrs, ctx} ->
      case Map.fetch(attrs, axis) do
        :error ->
          {attrs, ctx}

        {:ok, value} when is_float(value) ->
          {Map.put(attrs, axis, Emerge.UI.Scroll.validate_position!(value)), ctx}

        {:ok, %ScrollTarget{}} ->
          {attrs, ctx}

        {:ok, key} ->
          Emerge.UI.Scroll.validate_position!(key)

          {token, ctx} =
            case Map.get(old, axis) do
              %ScrollTarget{key: ^key, declaration: token} -> {token, ctx}
              _ -> alloc_id(ctx)
            end

          target = ctx |> Map.get(:new_key_ids, %{}) |> Map.get(key, 0)
          {Map.put(attrs, axis, %ScrollTarget{key: key, declaration: token, id: target}), ctx}
      end
    end)
  end

  defp keyed_index(old) do
    Enum.reduce(old, %{}, fn
      {_slot, %VNode{key: nil}}, acc -> acc
      {_slot, %VNode{key: key} = vnode}, acc -> Map.put(acc, key, vnode)
      %VNode{key: nil}, acc -> acc
      %VNode{key: key} = vnode, acc -> Map.put(acc, key, vnode)
    end)
  end

  defp reusable_root?(%VNode{kind: kind, key: key}, %Element{} = element) do
    kind == element.type and key == element_key(element)
  end

  # One Raven patch: the sibling-mode check walks the list with plain
  # recursion (no closures), and an empty list returns at once.
  defp children_mode([]), do: :unkeyed

  defp children_mode([first | rest]) do
    keyed? = has_key?(first)
    check_children_mode!(rest, keyed?)
    if keyed?, do: :keyed, else: :unkeyed
  end

  defp check_children_mode!([], _keyed?), do: :ok

  defp check_children_mode!([child | rest], keyed?) do
    if has_key?(child) != keyed?,
      do: raise(ArgumentError, "All siblings must have key when any key is provided")

    check_children_mode!(rest, keyed?)
  end

  defp nearby_mode([]), do: :unkeyed

  defp nearby_mode([{_slot, first} | rest]) do
    keyed? = has_key?(first)
    check_nearby_mode!(rest, keyed?)
    if keyed?, do: :keyed, else: :unkeyed
  end

  defp check_nearby_mode!([], _keyed?), do: :ok

  defp check_nearby_mode!([{_slot, element} | rest], keyed?) do
    if has_key?(element) != keyed?,
      do:
        raise(
          ArgumentError,
          "All nearby mounts on a host must have key when any key is provided"
        )

    check_nearby_mode!(rest, keyed?)
  end

  defp prepend_removed_children(old_children, used_old_ids, patches_rev, ctx) do
    Enum.reduce(old_children, {patches_rev, ctx}, fn child, {acc, ctx} ->
      if Map.has_key?(used_old_ids, child.id) do
        {acc, ctx}
      else
        {[{:remove, child.id} | acc], maybe_delete_event_tree(ctx, child)}
      end
    end)
  end

  defp prepend_removed_nearby(old_nearby, used_old_ids, patches_rev, ctx) do
    Enum.reduce(old_nearby, {patches_rev, ctx}, fn {_slot, vnode}, {acc, ctx} ->
      if Map.has_key?(used_old_ids, vnode.id) do
        {acc, ctx}
      else
        {[{:remove, vnode.id} | acc], maybe_delete_event_tree(ctx, vnode)}
      end
    end)
  end

  defp maybe_set_attrs(patches, _old, _new_attrs, _id, true), do: patches

  defp maybe_set_attrs(patches, %VNode{attrs: old_attrs}, new_attrs, id, false) do
    if old_attrs == new_attrs do
      patches
    else
      old_filtered = TreeAttrs.strip_runtime_attrs(old_attrs)
      new_filtered = TreeAttrs.strip_runtime_attrs(new_attrs)

      if old_filtered != new_filtered do
        Emerge.Engine.AttrValidation.validate_change_update!(old_filtered, new_filtered)
        [{:set_attrs, id, new_filtered} | patches]
      else
        patches
      end
    end
  end

  # One Raven patch: children and nearby mounts are usually the same nodes in
  # the same order; check that by walking both lists before building id lists.
  defp maybe_set_children(patches, %VNode{children: []}, []), do: patches

  defp maybe_set_children(patches, %VNode{id: id, children: old_children}, new_children) do
    if same_ids?(old_children, new_children),
      do: patches,
      else: set_children(patches, id, old_children, new_children)
  end

  defp set_children(patches, id, old_children, new_children) do
    old_ids = Enum.map(old_children, & &1.id)
    new_ids = Enum.map(new_children, & &1.id)

    if ordered_refs_need_set?(old_ids, new_ids, & &1) do
      [{:set_children, id, new_ids} | patches]
    else
      patches
    end
  end

  defp maybe_set_nearby_mounts(patches, %VNode{nearby: []}, []), do: patches

  defp maybe_set_nearby_mounts(
         patches,
         %VNode{id: id, nearby: old_nearby},
         new_nearby
       ) do
    if same_mounts?(old_nearby, new_nearby),
      do: patches,
      else: set_nearby_mounts(patches, id, old_nearby, new_nearby)
  end

  defp same_ids?([], []), do: true
  defp same_ids?([%VNode{id: id} | old], [%VNode{id: id} | new]), do: same_ids?(old, new)
  defp same_ids?(_old, _new), do: false

  defp same_mounts?([], []), do: true

  defp same_mounts?([{slot, %VNode{id: id}} | old], [{slot, %VNode{id: id}} | new]),
    do: same_mounts?(old, new)

  defp same_mounts?(_old, _new), do: false

  defp set_nearby_mounts(patches, id, old_nearby, new_nearby) do
    old_refs = mount_refs(old_nearby)
    new_refs = mount_refs(new_nearby)

    if ordered_refs_need_set?(old_refs, new_refs, fn {_slot, mount_id} -> mount_id end) do
      [{:set_nearby_mounts, id, new_refs} | patches]
    else
      patches
    end
  end

  defp ordered_refs_need_set?(old_refs, new_refs, key_fun) do
    old_refs != new_refs and do_ordered_refs_need_set?(old_refs, new_refs, key_fun)
  end

  defp do_ordered_refs_need_set?(old_refs, new_refs, key_fun)
       when length(old_refs) + length(new_refs) >= 32 do
    old_keys = MapSet.new(old_refs, key_fun)
    new_keys = MapSet.new(new_refs, key_fun)
    inserted_keys = MapSet.difference(new_keys, old_keys)
    removed_keys = MapSet.difference(old_keys, new_keys)

    if MapSet.size(inserted_keys) > 0 and MapSet.size(removed_keys) > 0 do
      true
    else
      old_remaining = Enum.reject(old_refs, &MapSet.member?(removed_keys, key_fun.(&1)))
      new_remaining = Enum.reject(new_refs, &MapSet.member?(inserted_keys, key_fun.(&1)))

      old_remaining != new_remaining
    end
  end

  defp do_ordered_refs_need_set?(old_refs, new_refs, key_fun) do
    old_keys = Enum.map(old_refs, key_fun)
    new_keys = Enum.map(new_refs, key_fun)

    inserted_keys = new_keys -- old_keys
    removed_keys = old_keys -- new_keys

    if inserted_keys != [] and removed_keys != [] do
      true
    else
      old_remaining = Enum.reject(old_refs, &(key_fun.(&1) in removed_keys))
      new_remaining = Enum.reject(new_refs, &(key_fun.(&1) in inserted_keys))

      old_remaining != new_remaining
    end
  end

  defp prepend_many(acc, []), do: acc

  defp prepend_many(acc, patches) when is_list(patches) do
    Enum.reverse(patches, acc)
  end

  defp alloc_id(%{next_id: id} = ctx) do
    {id, %{ctx | next_id: id + 1}}
  end

  defp mount_refs(nearby) do
    Enum.map(nearby, fn {slot, vnode} -> {slot, vnode.id} end)
  end

  defp element_key(%Element{key: key}) when not is_nil(key), do: key
  defp element_key(_), do: nil

  defp has_key?(%Element{key: key}) when not is_nil(key), do: true
  defp has_key?(_), do: false

  defp ensure_unique_key!(ctx, nil), do: ctx

  defp ensure_unique_key!(%{seen: seen} = ctx, key) do
    if MapSet.member?(seen, key) do
      raise ArgumentError, "duplicate explicit key: #{inspect(key)}"
    end

    %{ctx | seen: MapSet.put(seen, key)}
  end

  defp validate_viewport_root!(%Element{attrs: attrs}) do
    if Map.has_key?(attrs, :animate_exit) do
      raise ArgumentError, "animate_exit is not allowed on the viewport root"
    end
  end
end
