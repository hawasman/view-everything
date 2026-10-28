# One shared, persistent Index for search and sizes

Instead of an Everything-style name index plus a separate WizTree-style size scan, every Entry's name, parent, Logical Size, On-disk Size and modification time live in one persistent Index. Folder and Filtered Sizes are computed from that same Index. This is what lets a Query filter the tree, treemap and Category breakdown at the same time, which is the product's reason to exist. It costs memory: 8–16 extra bytes per Entry, targeting 10M Entries in about 1 GB.
