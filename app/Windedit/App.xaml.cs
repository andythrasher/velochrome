using Microsoft.UI.Xaml;

namespace Windedit;

public partial class App : Application
{
    MainWindow? window;

    public App() => InitializeComponent();

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // A photo or folder named on the command line opens straight away.
        var path = Environment.GetCommandLineArgs().Skip(1).FirstOrDefault(p => File.Exists(p) || Directory.Exists(p));
        window = new MainWindow(path);
        window.Activate();
    }
}
