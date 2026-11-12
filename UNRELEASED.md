## Fixed: Pages no longer reload in Safari from another computer

Opened in Safari on another computer of the network, Mynou loaded the whole
page again at every click: Safari keeps a page served over plain HTTP in a
separate, hardened process, and started a new one for each move to another
view. Views now change in place, as on the Mac running Mynou. The Skip to
content link also moves to the content again instead of opening the Overview.
