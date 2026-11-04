let
  isEppur = o: o.__eppur or false;
  before =
    list:
    if builtins.length list == 0 || isEppur (builtins.head list) then
      [ ]
    else
      [ (builtins.head list) ] ++ before (builtins.tail list);
in
{
  inherit before;
  wrap = mkOverlay: carry: args: {
    __eppur = true;
    __functor =
      _: final: prev:
      mkOverlay (args // { overlays = if carry then before (prev.overlays or [ ]) else [ ]; }) final prev;
  };
}
