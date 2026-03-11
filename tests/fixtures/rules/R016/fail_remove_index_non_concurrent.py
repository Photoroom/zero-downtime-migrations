# R016: RemoveIndex non-concurrent - should fail
from django.db import migrations


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0011'),
    ]

    operations = [
        migrations.RemoveIndex(
            model_name='order',
            name='order_created_idx',
        ),
    ]
