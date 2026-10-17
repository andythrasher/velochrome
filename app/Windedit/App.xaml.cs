using Microsoft.UI.Xaml;

namespace Windedit;

public partial class App : Application
{
    MainWindow? window;

    public App() => InitializeComponent();

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // A raw file named on the command line opens straight away.
        var path = Environment.GetCommandLineArgs().Skip(1).FirstOrDefault(File.Exists);
        window = new MainWindow(path);
        window.Activate();
    }
}
