# Siglus Performance Patch

A patch intended to improve the performance of the Siglus engine using hooks.

**DO NOT USE THIS PATCH UNLESS YOU UNDERSTAND WHAT IT DOES**

**PLEASE BACK UP YOUR SAVE DATA BEFORE TRYING IT AND WHILE USING IT. I AM NOT RESPONSIBLE FOR ANY DATA LOSS.**

This patch does the following to improve performance:

1. Hook `GetFileAttributesW`

    The VA programmers use an inefficient way to retrieve CGs in Extra mode, which calls `GetFileAttributesW` `37*11*4` times for every frame shown in Extra. This patch caches all file attributes in the `g00/` folder to speed up the process. However, it seems that the VM itself still consumes a significant amount of time during this operation.

2. Hook pack function

    Siglus uses LZSS for save data compression. Compared to modern compression algorithms, LZSS compression is relatively slow, which may cause lag during autosave. This patch skips compression when the original size exceeds 50 KB and performs packing asynchronously later in the save_to_file function.

3. Hook save-to-PNG function

    Possibly due to aliasing issues, the compiler appears unable to optimize the pixel copy loop properly — neither loop unrolling nor SIMD optimizations are applied. This results in a noticeable performance loss. This patch rewrites the routine.

4. Hook copy rotation

    During autosave, save files are rotated (e.g. `1008.sav -> 1009.sav`). This patch replaces the original implementation with `MoveFileW`.

5. Hook omv player

    The original OMV player is synchronous and often freezes frames. This patch hooks the player into our own implementation, which runs on a separate thread. The new player uses FFmpeg as its backend and comes with several improvements, like using AVX2 for pixel format conversion.

Currently, this project does not include any tools to modify the IAT to make the engine load the DLL automatically.

This project is for studying purposes only. Any other use is prohibited.