using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Microsoft.UI.Xaml.Controls;

namespace Windedit;

/// <summary>
/// The Rust engine (core/src/ffi.rs). Calls return at once; decoding and drawing happen on the
/// engine's threads, and each frame drawn raises <see cref="FrameDrawn"/> on the engine's GPU
/// thread (not the UI thread). Failed calls throw with the engine's message.
/// </summary>
public sealed unsafe partial class Engine : IDisposable
{
    const string Dll = "windedit_core";

    /// <summary>What a drawn frame showed (viewer.rs FrameEvent).</summary>
    [StructLayout(LayoutKind.Sequential)]
    public struct Frame
    {
        /// <summary>1: a frame was drawn. 2: the photo's thumbnail is ready (only Photo is set).</summary>
        public uint Kind;
        public int Photo;
        /// <summary>1 quick preview, 2 loupe preview, 3 full-size preview, 4 the raw.</summary>
        public uint Layer;
        /// <summary>The photo's size after rotation.</summary>
        public uint Width, Height;
        /// <summary>The raw's sensor size, once decoded (else 0).</summary>
        public uint RawWidth, RawHeight;
        public double SinceShowMs, GpuMs, CpuMs, OpenMs;
        /// <summary>For export events: the job, its state (0 working, 1 saved, 2 failed), how far it is
        /// (0–1) and a UTF-16 text valid only during the callback (see <see cref="ExportChanged"/>).</summary>
        public uint ExportId, ExportState;
        public float Progress;
        public IntPtr Text;
    }

    [LibraryImport(Dll)] private static partial IntPtr we_engine_create(delegate* unmanaged[Cdecl]<IntPtr, Frame*, void> callback, IntPtr user);
    [LibraryImport(Dll)] private static partial void we_engine_destroy(IntPtr engine);
    [LibraryImport(Dll)] private static partial uint we_last_error(IntPtr engine, char* buf, uint len);
    [LibraryImport(Dll)] private static partial uint we_adapter_name(IntPtr engine, char* buf, uint len);
    [LibraryImport(Dll)] private static partial double we_warm_up_ms(IntPtr engine);
    [LibraryImport(Dll, StringMarshalling = StringMarshalling.Utf16)] private static partial int we_open_folder(IntPtr engine, string path);
    [LibraryImport(Dll, StringMarshalling = StringMarshalling.Utf16)] private static partial int we_find_photo(IntPtr engine, string path);
    [LibraryImport(Dll)] private static partial uint we_photo_name(IntPtr engine, int index, char* buf, uint len);
    [LibraryImport(Dll)] private static partial int we_show(IntPtr engine, int index);
    [LibraryImport(Dll)] private static partial int we_set_zoom(IntPtr engine, int actual, float x, float y);
    [LibraryImport(Dll)] private static partial int we_set_edit_mode(IntPtr engine, int on);
    [LibraryImport(Dll)] private static partial int we_set_canvas_color(IntPtr engine, float r, float g, float b);
    [LibraryImport(Dll)] private static partial int we_view_resize(IntPtr engine, uint width, uint height, float scale, out IntPtr chain);
    [LibraryImport(Dll)] private static partial int we_prepare(IntPtr engine, uint width, uint height);
    [LibraryImport(Dll, StringMarshalling = StringMarshalling.Utf16)] private static partial int we_export(IntPtr engine, int index, string path, int quality);
    [LibraryImport(Dll, StringMarshalling = StringMarshalling.Utf16)] private static partial int we_save_snapshot(IntPtr engine, string path);
    [LibraryImport(Dll)] private static partial int we_get_metadata(IntPtr engine, Metadata* output, int count);
    [LibraryImport(Dll)] private static partial int we_get_edit(IntPtr engine, int index, out Edit edit);
    [LibraryImport(Dll)] private static partial int we_set_edit(IntPtr engine, int index, in Edit edit, int commit);
    [LibraryImport(Dll)] private static partial int we_undo(IntPtr engine, int index, int redo, out Edit edit);
    [LibraryImport(Dll)] private static partial int we_as_shot_white_balance(IntPtr engine, int index, out float kelvin, out float tint);
    [LibraryImport(Dll)] private static partial int we_set_rating(IntPtr engine, int index, int rating);
    [LibraryImport(Dll)] private static partial int we_set_pick(IntPtr engine, int index, int pick);
    [LibraryImport(Dll)] private static partial int we_set_label(IntPtr engine, int index, int label);
    [LibraryImport(Dll)] private static partial int we_set_order(IntPtr engine, int* order, int count);
    [LibraryImport(Dll)] private static partial int we_set_visible(IntPtr engine, int* visible, int count);
    [LibraryImport(Dll)] private static partial int we_thumbnail(IntPtr engine, int index, byte* buf, int capacity, out int width, out int height);

    public const uint FrameEvent = 1, ThumbnailEvent = 2, ExportEvent = 3;

    /// <summary>
    /// A photo's stars (0–5), pick flag (1 picked, -1 rejected, 0 neither) and color label
    /// (1–5: Red, Yellow, Green, Blue, Purple; 6 another name; 0 none). Saved in its XMP sidecar.
    /// </summary>
    [StructLayout(LayoutKind.Sequential)]
    public struct Metadata
    {
        public int Rating, Pick, Label;
        /// <summary>1 if the photo has adjustments.</summary>
        public int Edited;
    }

    /// <summary>
    /// A photo's adjustments (develop.rs Edit). All zero, with CustomWhiteBalance 0, is the photo
    /// as shot. Temperature (kelvin) and Tint apply only with CustomWhiteBalance 1; the rest are
    /// stops (Exposure) or −100 to 100.
    /// </summary>
    [StructLayout(LayoutKind.Sequential)]
    public struct Edit
    {
        public float Exposure;
        public uint CustomWhiteBalance;
        public float Temperature, Tint;
        public float Contrast, Highlights, Shadows, Whites, Blacks;
        public float Vibrance, Saturation;
    }

    IntPtr handle;
    GCHandle self;

    /// <summary>
    /// Raised on the engine's GPU thread after every frame drawn, and when a thumbnail is ready
    /// (see <see cref="Frame.Kind"/>).
    /// </summary>
    public event Action<Frame>? FrameDrawn;

    public Engine()
    {
        self = GCHandle.Alloc(this);
        handle = we_engine_create(&OnFrame, GCHandle.ToIntPtr(self));
        if (handle == IntPtr.Zero)
        {
            self.Free();
            throw new InvalidOperationException("This PC has no graphics card that supports Direct3D 12.");
        }
        AdapterName = ReadString(&we_adapter_name);
        WarmUpMs = we_warm_up_ms(handle);
    }

    /// <summary>An export's progress, or its end: <see cref="Text"/> is the file written when
    /// <see cref="State"/> is <see cref="ExportDone"/>, the problem when it is <see cref="ExportFailed"/>.</summary>
    public readonly record struct ExportUpdate(int Id, int Photo, int State, float Progress, string? Text, double DevelopMs, double ReadbackMs, double EncodeMs, double TotalMs);

    public const int ExportWorking = 0, ExportDone = 1, ExportFailed = 2;

    /// <summary>
    /// Raised on the engine's GPU thread when an export (<see cref="Export"/>) makes progress,
    /// finishes or fails.
    /// </summary>
    public event Action<ExportUpdate>? ExportChanged;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    static void OnFrame(IntPtr user, Frame* frame)
    {
        if (GCHandle.FromIntPtr(user).Target is not Engine engine)
            return;
        if (frame->Kind == ExportEvent)
        {
            // The text is only valid during this call, so it's read here.
            var text = frame->Text == IntPtr.Zero ? null : Marshal.PtrToStringUni(frame->Text);
            engine.ExportChanged?.Invoke(new ExportUpdate((int)frame->ExportId, frame->Photo, (int)frame->ExportState,
                frame->Progress, text, frame->GpuMs, frame->CpuMs, frame->OpenMs, frame->SinceShowMs));
            return;
        }
        engine.FrameDrawn?.Invoke(*frame);
    }

    public string AdapterName { get; }

    /// <summary>How long preparing the GPU kernels took at startup.</summary>
    public double WarmUpMs { get; }

    /// <summary>Opens a folder; returns how many photos it has.</summary>
    public int OpenFolder(string path) => Check(we_open_folder(handle, path));

    /// <summary>The index of the photo with this file, or -1.</summary>
    public int FindPhoto(string path) => Check(we_find_photo(handle, path));

    public string PhotoName(int index)
    {
        var buf = stackalloc char[260];
        var len = we_photo_name(handle, index, buf, 260);
        return new string(buf, 0, (int)Math.Min(len, 259));
    }

    public void Show(int index) => Check(we_show(handle, index));

    public void ZoomToFit() => Check(we_set_zoom(handle, 0, 0, 0));

    /// <summary>100% zoom centered on (x, y), as fractions of the photo's width and height.</summary>
    public void ZoomToActual(float x, float y) => Check(we_set_zoom(handle, 1, x, y));

    public Edit GetEdit(int index)
    {
        Check(we_get_edit(handle, index, out var edit));
        return edit;
    }

    /// <summary>
    /// Changes a photo's adjustments: <paramref name="commit"/> false while a slider is dragged
    /// (shown, not saved), true when it's let go (saved, and one step for undo).
    /// </summary>
    public void SetEdit(int index, Edit edit, bool commit) => Check(we_set_edit(handle, index, in edit, commit ? 1 : 0));

    /// <summary>Undoes (or redoes) a photo's last change: its adjustments now, or null if none.</summary>
    public Edit? Undo(int index, bool redo) => Check(we_undo(handle, index, redo ? 1 : 0, out var edit)) == 1 ? edit : null;

    /// <summary>The white balance as shot (kelvin, tint), once the raw is decoded.</summary>
    public (float Kelvin, float Tint)? AsShotWhiteBalance(int index) =>
        Check(we_as_shot_white_balance(handle, index, out var k, out var t)) == 1 ? (k, t) : null;

    /// <summary>
    /// Queues a full-size JPEG export of a photo, with its adjustments as they are now. If the
    /// name is taken the engine adds -2, -3, …; the file written comes with the done event.
    /// Returns the job's id, which <see cref="ExportChanged"/> carries.
    /// </summary>
    public int Export(int index, string path, int quality) => Check(we_export(handle, index, path, quality));

    public void SetEditMode(bool on) => Check(we_set_edit_mode(handle, on ? 1 : 0));

    /// <summary>The colour (0..1 each) around the photo, where the view has no picture.</summary>
    public void SetCanvasColor(float r, float g, float b) => Check(we_set_canvas_color(handle, r, g, b));

    /// <summary>Allocates GPU memory for photos with this sensor size, ahead of the first one.</summary>
    public void Prepare(uint width, uint height) => Check(we_prepare(handle, width, height));

    /// <summary>Every photo's metadata, in folder order.</summary>
    public Metadata[] GetMetadata(int count)
    {
        var all = new Metadata[count];
        fixed (Metadata* p = all)
            Check(we_get_metadata(handle, p, count));
        return all;
    }

    public void SetRating(int index, int rating) => Check(we_set_rating(handle, index, rating));

    public void SetPick(int index, int pick) => Check(we_set_pick(handle, index, pick));

    public void SetLabel(int index, int label) => Check(we_set_label(handle, index, label));

    /// <summary>The order the user moves through the photos, so the right ones are decoded ahead.</summary>
    public void SetOrder(ReadOnlySpan<int> order)
    {
        fixed (int* p = order)
            Check(we_set_order(handle, p, order.Length));
    }

    /// <summary>Photos whose thumbnails are on screen, most important first.</summary>
    public void SetVisible(ReadOnlySpan<int> visible)
    {
        fixed (int* p = visible)
            Check(we_set_visible(handle, p, visible.Length));
    }

    /// <summary>Thumbnail capacity: 256×256 BGRA.</summary>
    public const int ThumbnailBytes = 256 * 256 * 4;

    /// <summary>
    /// Copies a photo's thumbnail (BGRA, upright) into <paramref name="buffer"/>: 1 if copied,
    /// 0 if not ready yet, 2 if there won't be one.
    /// </summary>
    public int GetThumbnail(int index, Span<byte> buffer, out int width, out int height)
    {
        fixed (byte* p = buffer)
            return Check(we_thumbnail(handle, index, p, buffer.Length, out width, out height));
    }

    /// <summary>Saves the last frame drawn as a PNG (a diagnostic).</summary>
    public void SaveSnapshot(string path) => Check(we_save_snapshot(handle, path));

    /// <summary>Sizes the view's swap chain (physical pixels) and attaches it to the panel.</summary>
    public void ResizeView(SwapChainPanel panel, uint width, uint height, float scale)
    {
        Check(we_view_resize(handle, width, height, scale, out var chain));
        SetSwapChain(panel, chain);
    }

    public void Dispose()
    {
        if (handle != IntPtr.Zero)
        {
            // Waits for the engine's threads, so no callback comes after this.
            we_engine_destroy(handle);
            handle = IntPtr.Zero;
            self.Free();
        }
    }

    int Check(int result)
    {
        if (result < -1)
            throw new InvalidOperationException(ReadString(&we_last_error));
        return result;
    }

    string ReadString(delegate*<IntPtr, char*, uint, uint> read)
    {
        var buf = stackalloc char[512];
        var len = read(handle, buf, 512);
        return new string(buf, 0, (int)Math.Min(len, 511));
    }

    // Attaching is idempotent, so resizing simply attaches the (same) swap chain again.
    static readonly Guid ISwapChainPanelNative = new("63aad0b8-7c24-40ff-85a8-640d944cc325");

    static void SetSwapChain(SwapChainPanel panel, IntPtr chain)
    {
        var unknown = ((WinRT.IWinRTObject)panel).NativeObject.ThisPtr;
        var iid = ISwapChainPanelNative;
        Marshal.ThrowExceptionForHR(Marshal.QueryInterface(unknown, in iid, out var native));
        try
        {
            // ISwapChainPanelNative::SetSwapChain, after IUnknown's three methods.
            var setSwapChain = (delegate* unmanaged[Stdcall]<IntPtr, IntPtr, int>)(*(IntPtr**)native)[3];
            Marshal.ThrowExceptionForHR(setSwapChain(native, chain));
        }
        finally
        {
            Marshal.Release(native);
        }
    }
}
