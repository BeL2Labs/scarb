use std::env;
use std::fs;

use anyhow::{bail, ensure, Context, Result};
use cairo_lang_runner::short_string::as_cairo_short_string;
use cairo_lang_runner::{RunResultStarknet, RunResultValue, SierraCasmRunner, StarknetState};
use cairo_lang_sierra::program::VersionedProgram;
use camino::Utf8PathBuf;
use clap::Parser;
use indoc::formatdoc;
use serde::Serializer;

use scarb_metadata::{
    CompilationUnitMetadata, Metadata, MetadataCommand, PackageId, PackageMetadata, ScarbCommand,
};
use scarb_ui::args::{PackagesFilter, VerbositySpec};
use scarb_ui::components::Status;
use scarb_ui::{Message, OutputFormat, Ui};

mod deserialization;

/// Execute the main function of a package.
#[derive(Parser, Clone, Debug)]
#[command(author, version)]
struct Args {
    /// Name of the package.
    #[command(flatten)]
    packages_filter: PackagesFilter,

    /// Specify name of the function to run.
    #[arg(long)]
    function: Option<String>,

    /// Maximum amount of gas available to the program.
    #[arg(long)]
    available_gas: Option<usize>,

    /// Print more items in memory.
    #[arg(long, default_value_t = false)]
    print_full_memory: bool,

    /// Do not rebuild the package.
    #[arg(long, default_value_t = false)]
    no_build: bool,

    /// Logging verbosity.
    #[command(flatten)]
    pub verbose: VerbositySpec,

    /// Program arguments.
    ///
    /// This should be a JSON array of numbers, decimal bigints or recursive arrays of those. For example, pass `[1]`
    /// to the following function `fn main(a: u64)`, or pass `[1, "2"]` to `fn main(a: u64, b: u64)`,
    /// or `[1, 2, [3, 4, 5]]` to `fn main(t: (u64, u64), v: Array<u64>)`.
    #[arg(default_value = "[]")]
    arguments: deserialization::Args,
}

fn main() -> Result<()> {
    let args: Args = Args::parse();
    let ui = Ui::new(args.verbose.clone().into(), OutputFormat::Text);
    if let Err(err) = main_inner(&ui, args) {
        ui.anyhow(&err);
        std::process::exit(1);
    }
    Ok(())
}

fn main_inner(ui: &Ui, args: Args) -> Result<()> {
    let metadata = MetadataCommand::new().inherit_stderr().exec()?;

    let package = args.packages_filter.match_one(&metadata)?;

    let available_gas = GasLimit::parse(args.available_gas).with_metadata(&metadata, &package)?;

    if !args.no_build {
        let filter = PackagesFilter::generate_for::<Metadata>(vec![package.clone()].iter());
        ScarbCommand::new()
            .arg("build")
            .env("SCARB_PACKAGES_FILTER", filter.to_env())
            .run()?;
    }

    let filename = format!("{}.sierra.json", package.name);
    let path = Utf8PathBuf::from(env::var("SCARB_TARGET_DIR")?)
        .join(env::var("SCARB_PROFILE")?)
        .join(filename.clone());

    ensure!(
        path.exists(),
        formatdoc! {r#"
            package has not been compiled, file does not exist: {filename}
            help: run `scarb build` to compile the package
        "#}
    );

    ui.print(Status::new("Running", &package.name));

    let sierra_program = serde_json::from_str::<VersionedProgram>(
        &fs::read_to_string(path.clone())
            .with_context(|| format!("failed to read Sierra file: {path}"))?,
    )
    .with_context(|| format!("failed to deserialize Sierra program: {path}"))?
    .into_v1()
    .with_context(|| format!("failed to load Sierra program: {path}"))?;

    if available_gas.is_disabled() && sierra_program.program.requires_gas_counter() {
        bail!("program requires gas counter, please provide `--available-gas` argument");
    }

    let runner = SierraCasmRunner::new(
        sierra_program.program,
        if available_gas.is_disabled() {
            None
        } else {
            Some(Default::default())
        },
        Default::default(),
        None,
    )?;

    let function = args.function.as_deref().unwrap_or("main");
    let result = runner
        .run_function_with_starknet_context(
            runner.find_function(format!("::{function}").as_str())?,
            &args.arguments,
            available_gas.value(),
            StarknetState::default(),
        )
        .with_context(|| "failed to run the function")?;

    ui.print(Summary {
        result,
        print_full_memory: args.print_full_memory,
        gas_defined: available_gas.is_defined(),
    });

    Ok(())
}

struct Summary {
    result: RunResultStarknet,
    print_full_memory: bool,
    gas_defined: bool,
}

impl Message for Summary {
    fn print_text(self)
    where
        Self: Sized,
    {
        match self.result.value {
            RunResultValue::Success(values) => {
                println!("Run completed successfully, returning {values:?}")
            }
            RunResultValue::Panic(values) => {
                print!("Run panicked with [");
                for value in &values {
                    match as_cairo_short_string(value) {
                        Some(as_string) => print!("{value} ('{as_string}'), "),
                        None => print!("{value}, "),
                    }
                }
                println!("].")
            }
        }

        if self.gas_defined {
            if let Some(gas) = self.result.gas_counter {
                println!("Remaining gas: {gas}");
            }
        }

        if self.print_full_memory {
            print!("Full memory: [");
            for cell in &self.result.memory {
                match cell {
                    None => print!("_, "),
                    Some(value) => print!("{value}, "),
                }
            }
            println!("]");
        }
    }

    fn structured<S: Serializer>(self, _ser: S) -> Result<S::Ok, S::Error>
    where
        Self: Sized,
    {
        todo!("JSON output is not implemented yet for this command")
    }
}

enum GasLimit {
    Disabled,
    Unlimited,
    Limited(usize),
}

impl GasLimit {
    pub fn parse(value: Option<usize>) -> Self {
        match value {
            Some(0) => GasLimit::Disabled,
            Some(value) => GasLimit::Limited(value),
            None => GasLimit::Unlimited,
        }
    }

    /// Disable gas based on the compilation unit compiler config.
    pub fn with_metadata(self, metadata: &Metadata, package: &PackageMetadata) -> Result<Self> {
        let compilation_unit = metadata.package_lib_compilation_unit(package.id.clone());
        let cu_enables_gas = compilation_unit
            .map(|cu| cu.compiler_config.clone())
            .and_then(|c| {
                c.as_object()
                    .and_then(|c| c.get("enable_gas").and_then(|x| x.as_bool()))
            })
            // Defaults to true, meaning gas enabled - relies on cli config then.
            .unwrap_or(true);
        ensure!(
            cu_enables_gas || !self.is_defined(),
            "gas calculation disabled for package `{package_name}`, cannot define custom gas limit",
            package_name = package.name
        );
        if cu_enables_gas {
            // Leave unchanged.
            Ok(self)
        } else {
            // Disable gas based on CU config.
            Ok(GasLimit::Disabled)
        }
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self, GasLimit::Disabled)
    }

    /// Returns true if the gas limit has been defined by the user.
    pub fn is_defined(&self) -> bool {
        !matches!(self, GasLimit::Unlimited)
    }

    pub fn value(&self) -> Option<usize> {
        match self {
            GasLimit::Disabled => None,
            GasLimit::Limited(value) => Some(*value),
            GasLimit::Unlimited => Some(usize::MAX),
        }
    }
}

trait CompilationUnitProvider {
    /// Return the compilation unit for the package's lib target.
    fn package_lib_compilation_unit(
        &self,
        package_id: PackageId,
    ) -> Option<&CompilationUnitMetadata>;
}

impl CompilationUnitProvider for Metadata {
    fn package_lib_compilation_unit(
        &self,
        package_id: PackageId,
    ) -> Option<&CompilationUnitMetadata> {
        self.compilation_units
            .iter()
            .find(|m| m.package == package_id && m.target.kind == LIB_TARGET_KIND)
    }
}

const LIB_TARGET_KIND: &str = "lib";
