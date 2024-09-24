#!/usr/bin/env python3

import glob
import os
import pathlib
import shutil
import subprocess
import sys
import shlex
import tempfile
from argparse import ArgumentParser
from pathlib import Path as P

PARSER = ArgumentParser()
PARSER.add_argument('command', choices=['build', 'test', 'libs'])
PARSER.add_argument('build_dir', type=P)
PARSER.add_argument('src_dir', type=P)
PARSER.add_argument('root_dir', type=P)
PARSER.add_argument('target', choices=['release', 'debug'])
PARSER.add_argument('prefix', type=P)
PARSER.add_argument('libdir', type=P)
PARSER.add_argument('--version', default=None)
PARSER.add_argument('--bin', default=None, type=P)
PARSER.add_argument('--features', nargs="+", default=[])
PARSER.add_argument('--packages', nargs="+", default=[])
PARSER.add_argument('--examples', nargs="+", default=[])
PARSER.add_argument('--lib-suffixes', nargs="+", default=[])
PARSER.add_argument('--exe-suffix')
PARSER.add_argument('--depfile')
PARSER.add_argument('--disable-doc', action="store_true", default=False)


def shlex_join(args):
    if hasattr(shlex, 'join'):
        return shlex.join(args)
    return ' '.join([shlex.quote(arg) for arg in args])


def generate_depfile_for(fpath):
    # Get rid of the `.dll.lib` double suffixes first
    while fpath.suffixes:
        fpath = fpath.with_suffix('')
    file_stem = fpath.parent / fpath.stem
    depfile_content = ""
    with open(f"{file_stem}.d", 'r') as depfile:
        for l in depfile.readlines():
            if l.startswith(str(file_stem)):
                # We can't blindly split on `:` because on Windows that's part
                # of the drive letter. Lucky for us, the format of the dep file
                # is one of:
                #
                #   /path/to/output: /path/to/src1 /path/to/src2
                #   /path/to/output:
                #
                # So we parse these two cases specifically
                if l.endswith(':'):
                    output = l[:-1]
                    srcs = ''
                else:
                    output, srcs = l.split(": ", maxsplit=2)

                all_deps = []
                for src in srcs.split(" "):
                    all_deps.append(src)
                    src = P(src)
                    if src.name == 'lib.rs':
                        # `rustc` doesn't take `Cargo.toml` into account
                        # but we need to
                        cargo_toml = src.parent.parent / 'Cargo.toml'
                        if cargo_toml.exists():
                            all_deps.append(str(cargo_toml))

                depfile_content += f"{output}: {' '.join(all_deps)}\n"

    return depfile_content


def replace(line: str, rustlibs):
    if line.startswith('Libs.private:'):
        if rustlibs[0] not in line:
            line = line.strip() + ' ' + ' '.join(rustlibs) + '\n'
    return line


def patch_msvc_lib_file(src, dest_folder, sysroot, objcopy):
    src = pathlib.Path(src)
    pc_file = src.with_suffix('.pc')
    pc_file_uninstalled = src.with_stem(f'{src.stem}-uninstalled').with_suffix('.pc')
    dest = pathlib.Path(dest_folder) / src.name
    # if dest.exists():
    #     return  # assume patched
    # Parse output
    output = subprocess.check_output([
        'lib.exe', '/LIST', '/NOLOGO', src
    ]).decode().splitlines()
    # Which are definitely rustlibs
    stem = src.name.split('.')[0]
    rustlibs = set([i.split('.')[0] for i in output if (
        i.endswith('.rcgu.o') and not i.startswith(stem))])
    # Which are definitely the lib itself
    lib_itself = set(
        [i for i in output if i.startswith(stem) or i.endswith('.dll')]
    )
    # Repack the library
    with tempfile.TemporaryDirectory():
        for i in lib_itself:
            subprocess.check_call(['lib.exe', f'/EXTRACT:{i}', f'/OUT:{i}', '/NOLOGO',
                                   src])
            # Patch out rust_eh_personality -- aaaaaaaaaaaaaaaaaa
            subprocess.check_call(
                [objcopy, '--redefine-sym', f'rust_eh_personality={stem}_rust_eh_personality', i, i])
        subprocess.check_call(
            ['lib.exe', f'/OUT:{dest}', '/NOLOGO', *lib_itself])
    # Match up rustlibs (use MSVC syntax since they have a non-standard suffix)
    libs = [f'-L{sysroot.as_posix()}'] + [f'{i}.rlib' for i in rustlibs]

    if pc_file.exists():
        contents = pc_file.open('r', encoding='utf-8').readlines()
        contents = [replace(i, libs) for i in contents]
        pc_file.open('w', encoding='utf-8').writelines(contents)
    else:
        raise RuntimeError(f"File {pc_file.as_posix()} not found")

    # Also patch the meson-uninstalled file
    if pc_file_uninstalled.exists():
        contents = pc_file_uninstalled.open('r', encoding='utf-8').readlines()
        contents = [replace(i, libs) for i in contents]
        pc_file_uninstalled.open('w', encoding='utf-8').writelines(contents)
    else:
        raise RuntimeError(f"File {pc_file_uninstalled.as_posix()} not found")


if __name__ == "__main__":
    opts = PARSER.parse_args()
    logdir = opts.root_dir / 'meson-logs'
    logfile_path = logdir / f'{opts.src_dir.name}-cargo-wrapper.log'
    logfile = open(logfile_path, mode='w', buffering=1)

    print(opts, file=logfile)
    cargo_target_dir = opts.build_dir / 'target'

    env = os.environ.copy()
    if 'PKG_CONFIG_PATH' in env:
        pkg_config_path = env['PKG_CONFIG_PATH'].split(os.pathsep)
    else:
        pkg_config_path = []
    pkg_config_path.append(str(opts.root_dir / 'meson-uninstalled'))
    env['PKG_CONFIG_PATH'] = os.pathsep.join(pkg_config_path)

    if 'NASM' in env:
        env['PATH'] = os.pathsep.join([os.path.dirname(env['NASM']), env['PATH']])

    rustc_target = None
    if 'RUSTC' in env:
        rustc_cmdline = shlex.split(env['RUSTC'])
        # grab target from RUSTFLAGS
        rust_flags = rustc_cmdline[1:] + shlex.split(env.get('RUSTFLAGS', ''))
        if '--target' in rust_flags:
            rustc_target_idx = rust_flags.index('--target')
            _ = rust_flags.pop(rustc_target_idx)  # drop '--target'
            rustc_target = rust_flags.pop(rustc_target_idx)
        env['RUSTFLAGS'] = shlex_join(rust_flags)
        env['RUSTC'] = rustc_cmdline[0]

    features = opts.features
    if opts.command == 'build':
        cargo_cmd = ['cargo']
        if opts.bin or opts.examples:
            cargo_cmd += ['build']
        else:
            cargo_cmd += ['cbuild']
            if not opts.disable_doc:
                features += ['doc']
        if opts.target == 'release':
            cargo_cmd.append('--release')
    elif opts.command == 'test':
        # cargo test
        cargo_cmd = ['cargo', 'ctest', '--no-fail-fast', '--color=always']
    elif opts.command == 'libs':
        # Abort early -- get just the linker line as follows
        libroot = pathlib.Path(subprocess.check_output(['rustc',
                                                            '--print=target-libdir'],
                                                           env=env, cwd=opts.src_dir, encoding='utf-8').strip())
        if shutil.which('cl'):  # assume lld-link etc.
            linker_line = [f'/LIBPATH:{libroot.as_posix()}']
            for file in libroot.glob('*.rlib'):
                if file.name.startswith('libstd'):
                    # Link against it normally
                    linker_line.append(file.name)
                elif file.name.startswith('libcompiler_builtins'):
                    # This one is wholearchived
                    linker_line.append(f'/WHOLEARCHIVE:{file.name}')
        elif sys.platform == 'win32':
            linker_line = [f'-L{libroot.as_posix()}']
            for file in libroot.glob('*.rlib'):
                if file.name.startswith('libstd'):
                    # Link against it normally
                    linker_line.append('-l{file}')
                elif file.name.startswith('libcompiler_builtins'):
                    # This one is wholearchived
                    linker_line.append(f'-Wl,--whole-archive,{file.as_posix()}')
        print(';'.join(linker_line))
        sys.exit(0)
    else:
        print("Unknown command:", opts.command, file=logfile)
        sys.exit(1)

    if rustc_target:
        cargo_cmd += ['--target', rustc_target]
    if features:
        cargo_cmd += ['--features', ','.join(features)]
    cargo_cmd += ['--target-dir', cargo_target_dir]
    cargo_cmd += ['--manifest-path', opts.src_dir / 'Cargo.toml']
    if opts.bin:
        cargo_cmd.extend(['--bin', opts.bin.name])
    else:
        if not opts.examples:
            cargo_cmd.extend(['--prefix', opts.prefix, '--libdir',
                              opts.prefix / opts.libdir])
        for p in opts.packages:
            cargo_cmd.extend(['-p', p])
        for e in opts.examples:
            cargo_cmd.extend(['--example', e])
        if opts.lib_suffixes == ['lib']:
            cargo_cmd.extend(['--library-type', 'staticlib'])
        elif opts.lib_suffixes == ['dll']:
            cargo_cmd.extend(['--library-type', 'cdylib'])

    def run(cargo_cmd, env):
        print(cargo_cmd, env, file=logfile)
        try:
            subprocess.run(cargo_cmd, env=env, cwd=opts.src_dir, check=True)
        except subprocess.SubprocessError:
            sys.exit(1)

    run(cargo_cmd, env)

    if opts.command == 'build':
        target_dir = cargo_target_dir / '**' / opts.target
        if opts.bin:
            exe = glob.glob(str(target_dir / opts.bin) + opts.exe_suffix, recursive=True)[0]
            shutil.copy2(exe, opts.build_dir)
            depfile_content = generate_depfile_for(P(exe))
        else:
            # Copy so files to build dir
            depfile_content = ""
            libroot = pathlib.Path(subprocess.check_output(['rustc',
                                                            '--print=target-libdir'],
                                                           env=env, cwd=opts.src_dir, encoding='utf-8').strip()
                                   )
            sysroot = pathlib.Path(
                subprocess.check_output(['rustc',
                                         '--print=sysroot'],
                                        env=env, cwd=opts.src_dir, encoding='utf-8').strip()
            )
            llvm_objcopy = shutil.which('ar')
            if not llvm_objcopy:
                llvm_objcopy = list(sysroot.glob('**/llvm-objcopy.exe'))
                if len(llvm_objcopy) == 0 or not llvm_objcopy[0].exists():
                    raise RuntimeError(
                        'Please install the llvm-tools component')
                else:
                    llvm_objcopy = llvm_objcopy[0]

            for suffix in opts.lib_suffixes:
                for f in glob.glob(str(target_dir / f'*.{suffix}'), recursive=True):
                    libfile = P(f)

                    depfile_content += generate_depfile_for(libfile)

                    copied_file = (opts.build_dir / libfile.name)
                    try:
                        if copied_file.stat().st_mtime == libfile.stat().st_mtime:
                            print(f"{copied_file} has not changed.", file=logfile)
                            continue
                    except FileNotFoundError:
                        pass

                    print(f"Copying {copied_file}", file=logfile)
                    if suffix == 'lib':
                        patch_msvc_lib_file(
                            f, opts.build_dir, sysroot=libroot, objcopy=llvm_objcopy)
                    else:
                        shutil.copy2(f, opts.build_dir)
            # Copy examples to builddir
            for example in opts.examples:
                example_glob = str(target_dir / 'examples' / example) + opts.exe_suffix
                exe = glob.glob(example_glob, recursive=True)[0]
                shutil.copy2(exe, opts.build_dir)
                depfile_content += generate_depfile_for(P(exe))

        with open(opts.depfile, 'w') as depfile:
            depfile.write(depfile_content)

        # Copy generated pkg-config files
        for f in glob.glob(str(target_dir / '*.pc'), recursive=True):
            shutil.copy(f, opts.build_dir)

        # Move -uninstalled.pc to meson-uninstalled
        uninstalled = opts.build_dir / 'meson-uninstalled'
        os.makedirs(uninstalled, exist_ok=True)

        for f in opts.build_dir.glob('*-uninstalled.pc'):
            # move() does not allow us to update the file so remove it if it already exists
            dest = uninstalled / P(f).name
            if dest.exists():
                dest.unlink()
            # move() takes paths from Python3.9 on
            if ((sys.version_info.major >= 3) and (sys.version_info.minor >= 9)):
                shutil.move(f, uninstalled)
            else:
                shutil.move(str(f), str(uninstalled))

