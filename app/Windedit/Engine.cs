using System.Runtime.InteropServices;
using Microsoft.UI.Xaml.Controls;

namespace Windedit;

/// <summary>
/// The Rust engine (core/src/ffi.rs). Calls return 0 on success; anything else throws with the
/// engine's message. Not thread-safe: use it from one thread at a time.
/// </summary>
sealed unsafe partial class Engine : IDisposable
{
    const string Dll = "windedit_core";

    [StructLayout(LayoutKind.Sequential)]
    public struct OpenInfo
    {
        public uint Width, Height;
        public double DecodeMs, UploadMs, CopyMs, DemosaicMs, PyramidMs, TotalMs;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct FrameTimings
    {
        public double DevelopMs, GpuMs, CpuMs;
    }

    [LibraryImport(Dll)] private static partial IntPtr we_engine_create();
    [LibraryImport(Dll)] private static partial void we_engine_destroy(IntPtr engine);
    [LibraryImport(Dll)] private static partial uint we_last_error(IntPtr engine, char* buf, uint len);
    [LibraryImport(Dll)] private static partial uint we_adapter_name(IntPtr engine, char* buf, uint len);
    [LibraryImport(Dll, StringMarshalling = StringMarshalling.Utf16)] private static partial int we_open(IntPtr engine, string path, out OpenInfo info);
    [LibraryImport(Dll)] private static partial int we_view_resize(IntPtr engine, uint width, uint height, float scale, out IntPtr chain);
    [LibraryImport(Dll)] private static partial int we_set_exposure(IntPtr engine, float stops);
    [LibraryImport(Dll)] private static partial int we_set_zoom(IntPtr engine, int actual, float x, float y);
    [LibraryImport(Dll)] private static partial int we_render(IntPtr engine, out FrameTimings timings);

    IntPtr handle;

    public Engine()
    {
        handle = we_engine_create();
        if (handle == IntPtr.Zero)
            throw new InvalidOperationException("This PC has no graphics card that supports Direct3D 12.");
        AdapterName = ReadString(&we_adapter_name);
    }

    public string AdapterName { get; }

    public OpenInfo Open(string path)
    {
        Check(we_open(handle, path, out var info));
        return info;
    }

    /// <summary>Sizes the view's swap chain (physical pixels) and attaches it to the panel.</summary>
    public void ResizeView(SwapChainPanel panel, uint width, uint height, float scale)
    {
        Check(we_view_resize(handle, width, height, scale, out var chain));
        SetSwapChain(panel, chain);
    }

    public void SetExposure(float stops) => Check(we_set_exposure(handle, stops));

    public void ZoomToFit() => Check(we_set_zoom(handle, 0, 0, 0));

    /// <summary>100% zoom centered on (x, y), as fractions of the photo's width and height.</summary>
    public void ZoomToActual(float x, float y) => Check(we_set_zoom(handle, 1, x, y));

    public FrameTimings Render()
    {
        Check(we_render(handle, out var timings));
        return timings;
    }

    public void Dispose()
    {
        if (handle != IntPtr.Zero)
        {
            we_engine_destroy(handle);
            handle = IntPtr.Zero;
        }
    }

    void Check(int result)
    {
        if (result != 0)
            throw new InvalidOperationException(ReadString(&we_last_error));
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
