using Microsoft.UI.Xaml;

namespace Windedit;

public partial class App : Application
{
    static readonly string CrashLog = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Windedit", "crash.log");

    MainWindow? window;

    public App()
    {
        InitializeComponent();
        // Record crashes, but still crash: nothing here marks them handled.
        UnhandledException += (_, e) => LogCrash("XAML", e.Exception);
        AppDomain.CurrentDomain.UnhandledException += (_, e) => LogCrash("AppDomain", e.ExceptionObject as Exception);
        TaskScheduler.UnobservedTaskException += (_, e) => LogCrash("unobserved task", e.Exception);
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // A photo or folder named on the command line opens straight away.
        var path = Environment.GetCommandLineArgs().Skip(1).FirstOrDefault(p => File.Exists(p) || Directory.Exists(p));
        window = new MainWindow(path);
        window.Activate();
    }

    static void LogCrash(string source, Exception? ex)
    {
        try
        {
            var text = $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} ({source})\n";
            for (var e = ex; e != null; e = e.InnerException)
                text += $"{e.GetType().FullName} 0x{e.HResult:X8}: {e.Message}\n{e.StackTrace}\n";
            Directory.CreateDirectory(Path.GetDirectoryName(CrashLog)!);
            File.AppendAllText(CrashLog, text + "\n");
        }
        catch { }
    }
}
